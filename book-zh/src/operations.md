# 运维操作

本章涵盖日常运维任务：升级、凭据管理和服务管理。

---

## 升级

拉取最新源码并重新构建：

```bash
cd octos
git pull origin main
./scripts/local-deploy.sh --full   # Rebuilds and reinstalls
```

如果以服务方式运行，升级后需要重启：

```bash
# macOS (launchd):
launchctl unload ~/Library/LaunchAgents/io.octos.octos-serve.plist
launchctl load ~/Library/LaunchAgents/io.octos.octos-serve.plist

# Linux (systemd):
systemctl --user restart octos-serve
```

---

## 钥匙串集成

Octos 支持将 API 密钥存储在 macOS 钥匙串中，而不是以明文形式存放在配置文件的 JSON 中。这在 Apple Silicon 上提供硬件级加密和操作系统级别的访问控制。

### 架构

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

**解析链**：配置文件中的 `"keychain:"` 标记触发钥匙串查找（3 秒超时）。如果钥匙串不可用，该密钥会被跳过并输出警告。

**向后兼容**：`env_vars` 中的字面值直接透传。无需迁移 -- 可以按需逐个密钥切换到钥匙串。完全支持明文和钥匙串条目混合使用。

### CLI 命令

```bash
# 为 SSH 会话解锁钥匙串（通过 SSH 使用 set-key 前必须执行）
octos auth unlock --password <login-password>
octos auth unlock                               # interactive prompt

# 将密钥存入钥匙串并更新配置文件使用 keychain 标记
octos auth set-key OPENAI_API_KEY sk-proj-abc123
octos auth set-key OPENAI_API_KEY              # interactive prompt

# 指定配置文件
octos auth set-key GEMINI_API_KEY AIzaSy... -p my-profile

# 列出所有密钥及其存储状态
octos auth keys
octos auth keys -p my-profile

# 从钥匙串移除并清理配置文件
octos auth remove-key OPENAI_API_KEY
```

### 钥匙串条目格式

- **Service**：`octos`（所有条目使用相同常量）
- **Account**：环境变量名（例如 `OPENAI_API_KEY`）
- **Password**：实际的密钥值

验证方法：

```bash
security find-generic-password -s octos -a OPENAI_API_KEY -w
```

### SSH 和无头服务器设置

macOS 钥匙串绑定到 GUI 登录会话。SSH 会话无法访问已锁定的钥匙串 -- macOS 会尝试弹出对话框，在无头服务器上这会导致卡死。

**为什么 SSH 默认无法访问**：macOS `securityd` 按会话解锁钥匙串。GUI 会话的解锁不会自动传播到 SSH 会话。

**解决方案**：解锁钥匙串并禁用自动锁定。每次启动执行一次（或加入部署脚本）：

```bash
ssh user@<host>

# 解锁钥匙串（需要登录密码）
octos auth unlock --password <login-password>

# 完成 -- 自动锁定已自动禁用。
# 钥匙串保持解锁状态直到重启。
# 自动登录会在重启时重新解锁。
```

或使用原生 `security` 命令：

```bash
# 解锁
security unlock-keychain -p '<password>' ~/Library/Keychains/login.keychain-db

# 禁用自动锁定计时器（防止空闲后重新锁定）
security set-keychain-settings ~/Library/Keychains/login.keychain-db
```

**常见问题：**

| 现象 | 原因 | 解决方法 |
|---------|-------|-----|
| "User interaction is not allowed" | 钥匙串已锁定（SSH 会话） | `octos auth unlock --password <pw>` |
| 钥匙串查找超时（3 秒） | 钥匙串已锁定（LaunchAgent） | 启用自动登录，重启 |
| "keychain marker found but no secret" | 密钥未存储或使用了错误的钥匙串 | 解锁后重新执行 `octos auth set-key` |
| 网关启动时卡住 | 钥匙串查找阻塞 | 更新到最新的 octos 二进制文件 |

### 安全性对比

| 威胁场景 | 明文 JSON | 钥匙串 |
|--------|---------------|----------|
| 文件被窃取（备份、git、scp） | 所有密钥暴露 | 只能看到 `"keychain:"` 标记 |
| 恶意软件读取磁盘 | 简单文件读取即可获取密钥 | 必须绕过操作系统钥匙串 ACL |
| 机器上的其他用户 | 文件权限有一定保护，root 可读 | 按用户加密 |
| 进程内存转储 | 密钥在环境变量中 | 密钥仅短暂存在于内存中 |
| 意外日志输出 | 配置文件 JSON 泄露密钥 | 仅记录引用字符串 |

### 服务器部署建议

macOS 钥匙串是为桌面交互使用设计的。在无头服务器上，它会引入可靠性问题。请根据部署类型选择凭据存储方式：

| 部署场景 | 推荐存储方式 | 原因 |
|------------|-------------------|--------|
| **开发者笔记本** | 钥匙串（`"keychain:"`） | GUI 会话保持钥匙串解锁；ACL 弹窗可以接受 |
| **自动登录 + GUI 的 Mac** | 钥匙串（`"keychain:"`） | 如果通过屏幕共享批准过 ACL 对话框则可用 |
| **无头 Mac（仅 SSH）** | `env_vars` 或 launchd plist 中的明文 | 最可靠；无解锁/ACL 依赖 |
| **Linux 服务器** | 环境变量中的明文 | 没有 macOS 钥匙串 |

**为什么钥匙串在无头服务器上不可靠：**

1. **需要 macOS 登录密码** -- 通过 SSH 解锁钥匙串需要用户的登录密码存储在某处，降低了安全收益。
2. **重启/休眠后重新锁定** -- 启动 `octos serve` 的 LaunchAgent 在 GUI 登录之前运行，此时钥匙串处于锁定状态。
3. **空闲超时后重新锁定** -- 即使解锁后，macOS 也可能重新锁定。`set-keychain-settings` 的变通方案可能被 macOS 更新重置。
4. **ACL 弹窗阻断无头访问** -- 如果二进制文件不是最初存储密钥的那个，macOS 可能弹出一个无法回答的 GUI 对话框。
5. **会话隔离** -- 从 SSH 解锁不会解锁 LaunchAgent 会话的钥匙串，反之亦然。

**服务器的明文设置：**

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

使用文件系统权限保护文件：

```bash
chmod 600 ~/.octos/profiles/*.json
chmod 600 ~/Library/LaunchAgents/io.octos.octos-serve.plist
```

---

## 服务管理

### macOS (launchd)

创建 LaunchAgent plist 将 octos 作为持久服务运行：

```bash
# 加载服务
launchctl load ~/Library/LaunchAgents/io.octos.octos-serve.plist

# 卸载服务
launchctl unload ~/Library/LaunchAgents/io.octos.octos-serve.plist

# 检查状态
launchctl list | grep octos
```

如果服务需要环境变量（例如 SMTP 凭据），将其添加到 plist 中：

```xml
<key>EnvironmentVariables</key>
<dict>
    <key>SMTP_PASSWORD</key>
    <string>xxxx xxxx xxxx xxxx</string>
</dict>
```

日志位于 `~/.octos/serve.log`。

### Linux (systemd)

使用 systemd 用户单元管理服务：

```bash
# 启动 / 停止 / 重启
systemctl --user start octos-serve
systemctl --user stop octos-serve
systemctl --user restart octos-serve

# 设置开机自启
systemctl --user enable octos-serve

# 查看状态和日志
systemctl --user status octos-serve
journalctl --user -u octos-serve
```
