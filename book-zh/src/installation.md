# 安装与部署

## 前置条件

| 条件 | 版本 | 备注 |
|------|------|------|
| Rust | 1.85.0+ | 通过 [rustup.rs](https://rustup.rs) 安装 |
| macOS | 13+ | Apple Silicon 或 Intel |
| Linux | glibc 2.31+ | Ubuntu 20.04+、Debian 11+、Fedora 34+ |
| Windows | 10/11 | 原生编译或 WSL2 |

你还需要至少一个受支持的 LLM 供应商的 API 密钥。

### 可选依赖

| 依赖 | 用途 | 安装方式 |
|------|------|----------|
| Node.js | WhatsApp 桥接、PPTX 创建技能 | `brew install node` / `apt install nodejs` |
| ffmpeg | 媒体/视频技能 | `brew install ffmpeg` / `apt install ffmpeg` |
| Chrome/Chromium | 浏览器自动化工具 | `brew install --cask chromium` |
| LibreOffice | Office 文档转换 | `brew install --cask libreoffice` |
| Poppler | PDF 渲染（`pdftoppm`） | `brew install poppler` / `apt install poppler-utils` |

## 从源码编译

```bash
git clone https://github.com/octos-org/octos
cd octos

# 基本功能（CLI、chat、run、gateway + CLI 渠道）
cargo install --path crates/octos-cli

# 启用消息渠道
cargo install --path crates/octos-cli --features telegram,discord,slack,whatsapp,feishu,email,wecom

# 启用浏览器自动化（需要 Chrome/Chromium）
cargo install --path crates/octos-cli --features browser

# 启用 Web 界面和 REST API
cargo install --path crates/octos-cli --features api

# 验证安装
octos --version
```

## 部署脚本

使用部署脚本可以简化安装流程：

```bash
# 最小安装（仅 CLI + 对话）
./scripts/local-deploy.sh --minimal

# 完整安装（所有渠道 + 仪表板 + 应用技能）
./scripts/local-deploy.sh --full

# 自定义渠道
./scripts/local-deploy.sh --channels telegram,discord,api
```

## 各平台安装指南

### macOS

```bash
# 1. 安装 Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# 2. 安装可选依赖
brew install node ffmpeg poppler
brew install --cask libreoffice

# 3. 克隆并部署
git clone https://github.com/octos-org/octos.git
cd octos
./scripts/local-deploy.sh --full

# 4. 设置 API 密钥并运行
export ANTHROPIC_API_KEY=sk-ant-...
octos chat
```

**后台服务（launchd）：**

部署脚本会创建 `~/Library/LaunchAgents/io.octos.octos-serve.plist`。

```bash
# 启动服务（重启后自动恢复）
launchctl load ~/Library/LaunchAgents/io.octos.octos-serve.plist

# 停止服务
launchctl unload ~/Library/LaunchAgents/io.octos.octos-serve.plist

# 查看日志
tail -f ~/.octos/serve.log
```

### Linux (Ubuntu/Debian)

```bash
# 1. 安装系统依赖
sudo apt update
sudo apt install -y build-essential pkg-config libssl-dev

# 2. 安装 Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# 3. 安装可选依赖
sudo apt install -y nodejs npm ffmpeg poppler-utils

# 4. 克隆并部署
git clone https://github.com/octos-org/octos.git
cd octos
./scripts/local-deploy.sh --full

# 5. 设置 API 密钥并运行
export ANTHROPIC_API_KEY=sk-ant-...
octos chat
```

**后台服务（systemd 用户单元）：**

部署脚本会创建 `~/.config/systemd/user/octos-serve.service`。

```bash
# 启动服务
systemctl --user start octos-serve

# 开机自启（需要启用 lingering）
loginctl enable-linger $USER
systemctl --user enable octos-serve

# 查看日志
journalctl --user -u octos-serve -f

# 停止服务
systemctl --user stop octos-serve
```

### Linux (Fedora/RHEL)

```bash
# 安装系统依赖
sudo dnf install -y gcc pkg-config openssl-devel

# 然后按照上方 Ubuntu 的步骤从第 2 步开始操作
```

### Windows（原生）

Octos 支持在 Windows 上原生编译和运行。Shell 命令通过 `cmd /C` 执行。

```powershell
# 1. 安装 Rust（从 https://rustup.rs 下载 rustup-init.exe）
rustup-init.exe

# 2. 克隆并编译
git clone https://github.com/octos-org/octos.git
cd octos
cargo install --path crates/octos-cli

# 3. 设置 API 密钥并运行
$env:ANTHROPIC_API_KEY = "sk-ant-..."
octos chat
```

**Windows 注意事项：**

- Windows 上沙箱功能不可用（没有 bubblewrap/sandbox-exec 的等效工具）；Shell 命令在无隔离环境下运行。如果安装了 Docker Desktop，Docker 沙箱模式仍然可用。
- API 密钥通过 Windows 凭据管理器存储。
- 进程管理使用 `taskkill` 进行清理。

### Windows (WSL2)

也可以使用 WSL2 获得 Linux 环境：

```powershell
# 1. 安装 WSL2（以管理员身份运行 PowerShell）
wsl --install -d Ubuntu

# 2. 打开 Ubuntu 终端，然后按照上方 Linux (Ubuntu) 的步骤操作
```

在 WSL2 中运行 `octos serve` 时，可以通过 Windows 浏览器访问 `http://localhost:8080`（WSL2 自动转发端口）。

## Docker

```bash
docker compose --profile gateway up -d
```

## 部署脚本参考

```
./scripts/local-deploy.sh [OPTIONS]

Options:
  --minimal          仅 CLI + 对话（不含渠道和仪表板）
  --full             所有渠道 + 仪表板 + 应用技能
  --channels LIST    逗号分隔的渠道列表：telegram,discord,slack,whatsapp,feishu,email,twilio,wecom
  --no-skills        跳过编译应用技能
  --no-service       跳过 launchd/systemd 服务配置
  --uninstall        移除二进制文件和服务文件
  --debug            以 debug 模式编译（编译更快，二进制更大）
  --prefix DIR       安装路径前缀（默认：~/.cargo/bin）
```

在 Windows 上使用 `.\scripts\local-deploy.ps1`（PowerShell），选项相同。

**脚本执行流程：**

1. 检查前置条件（Rust、平台依赖）
2. 使用所选特性编译 `octos` 二进制文件
3. 编译应用技能二进制文件（除非指定了 `--no-skills`）
4. 在 macOS 上对二进制文件进行签名（ad-hoc codesign）
5. 如果 `~/.octos` 不存在，运行 `octos init`
6. 创建后台服务文件（macOS 用 launchd，Linux 用 systemd）

**卸载：**

```bash
./scripts/local-deploy.sh --uninstall
# 数据目录（~/.octos）不会被移除。如需删除请手动执行：
rm -rf ~/.octos
```

## 安装后验证

### 设置 API 密钥

至少设置一个 LLM 供应商的密钥：

```bash
# 添加到 ~/.bashrc、~/.zshrc 或 ~/.profile
export ANTHROPIC_API_KEY=sk-ant-...
# 或
export OPENAI_API_KEY=sk-...
# 或使用 OAuth 登录
octos auth login --provider openai
```

### 验证

```bash
octos --version              # 检查二进制文件
octos status                 # 检查配置和 API 密钥
octos chat --message "Hello" # 快速测试
```

## 升级

```bash
cd octos
git pull origin main
./scripts/local-deploy.sh --full   # 重新编译并安装

# 如果以服务方式运行，需要重启：
# macOS：
launchctl unload ~/Library/LaunchAgents/io.octos.octos-serve.plist
launchctl load ~/Library/LaunchAgents/io.octos.octos-serve.plist
# Linux：
systemctl --user restart octos-serve
```

## 常见问题

| 问题 | 解决方案 |
|------|----------|
| `octos: command not found` | 将 `~/.cargo/bin` 加入 PATH：`export PATH="$HOME/.cargo/bin:$PATH"` |
| Linux 上编译失败 | 安装 `build-essential pkg-config libssl-dev` |
| macOS 代码签名警告 | 执行：`codesign -s - ~/.cargo/bin/octos` |
| 无法访问仪表板 | 检查端口：`octos serve --port 8080`，打开 `http://localhost:8080` |
| WSL2 端口未转发 | 重启 WSL：`wsl --shutdown`，然后重新打开终端 |
| 服务无法启动 | 检查日志：`tail -f ~/.octos/serve.log` 或 `journalctl --user -u octos-serve` |
| 找不到 API 密钥 | 确保环境变量在服务环境中已设置，而不仅仅在你的 Shell 中 |
