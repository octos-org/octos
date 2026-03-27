# 网关与频道

Octos 以**网关**模式运行，将各消息平台桥接到你的 LLM 智能体。每个平台连接称为一个**频道**。你可以在同一个网关进程中同时运行多个频道——例如同时接入 Telegram 和 Slack。

## 频道概览

频道在 `config.json` 的 `gateway.channels` 数组中配置。每个条目指定一个 `type`、可选的 `allowed_senders` 用于访问控制，以及平台特定的 `settings`。

查看已编译和已配置的频道：

```bash
octos channels status
```

该命令会显示一张表格，列出每个频道的编译状态（feature flags）和配置摘要（环境变量的设置情况）。

---

## Telegram

需要从 [@BotFather](https://t.me/BotFather) 获取 bot token。

```bash
export TELEGRAM_BOT_TOKEN="123456:ABC..."
```

```json
{
  "type": "telegram",
  "allowed_senders": ["your_user_id"],
  "settings": {
    "token_env": "TELEGRAM_BOT_TOKEN"
  }
}
```

Telegram 支持 bot 命令、内联键盘、语音消息、图片和文件。

---

## Slack

需要一个 Socket Mode 应用，同时提供 bot token 和 app-level token。

```bash
export SLACK_BOT_TOKEN="xoxb-..."
export SLACK_APP_TOKEN="xapp-..."
```

```json
{
  "type": "slack",
  "settings": {
    "bot_token_env": "SLACK_BOT_TOKEN",
    "app_token_env": "SLACK_APP_TOKEN"
  }
}
```

---

## Discord

需要从 [Discord Developer Portal](https://discord.com/developers/applications) 获取 bot token。

```bash
export DISCORD_BOT_TOKEN="..."
```

```json
{
  "type": "discord",
  "settings": {
    "token_env": "DISCORD_BOT_TOKEN"
  }
}
```

---

## WhatsApp

需要一个运行在 WebSocket URL 上的 Node.js 桥接（Baileys）。

```json
{
  "type": "whatsapp",
  "settings": {
    "bridge_url": "ws://localhost:3001"
  }
}
```

---

## 飞书（中国版）

飞书默认使用 WebSocket 长连接模式（无需公网 URL）。

```bash
export FEISHU_APP_ID="cli_..."
export FEISHU_APP_SECRET="..."
```

```json
{
  "type": "feishu",
  "settings": {
    "app_id_env": "FEISHU_APP_ID",
    "app_secret_env": "FEISHU_APP_SECRET"
  }
}
```

构建时需启用 `feishu` feature flag：

```bash
cargo build --release -p octos-cli --features feishu
```

---

## Lark（国际版）

Lark（国际版飞书）**不支持** WebSocket 模式，需改用 webhook 模式——由 Lark 通过 HTTP POST 将事件推送到你的服务器。

```
Lark Cloud --> ngrok --> localhost:9321/webhook/event --> Gateway --> LLM
```

### 开发者控制台配置

1. 前往 [open.larksuite.com/app](https://open.larksuite.com/app)，创建或选择一个应用
2. 在 Features 下添加 **Bot** 能力
3. 配置事件订阅：
   - Events & Callbacks > Event Configuration > Edit subscription method
   - 选择 "Send events to developer server"
   - 将请求 URL 设为 `https://YOUR_NGROK_URL/webhook/event`
4. 添加事件：`im.message.receive_v1`（接收消息）
5. 启用权限：`im:message`、`im:message:send_as_bot`、`im:resource`
6. 发布应用：App Release > Version Management > Create Version > Apply for Online Release

### 配置

```bash
export LARK_APP_ID="cli_..."
export LARK_APP_SECRET="..."
```

```json
{
  "type": "lark",
  "allowed_senders": [],
  "settings": {
    "app_id_env": "LARK_APP_ID",
    "app_secret_env": "LARK_APP_SECRET",
    "region": "global",
    "mode": "webhook",
    "webhook_port": 9321
  }
}
```

### 配置项参考

| 配置项 | 说明 | 默认值 |
|---------|-------------|---------|
| `app_id_env` | App ID 的环境变量名 | `FEISHU_APP_ID` |
| `app_secret_env` | App Secret 的环境变量名 | `FEISHU_APP_SECRET` |
| `region` | `"cn"`（飞书）或 `"global"` / `"lark"`（Lark 国际版） | `"cn"` |
| `mode` | `"ws"`（WebSocket）或 `"webhook"`（HTTP） | `"ws"` |
| `webhook_port` | webhook HTTP 服务端口 | `9321` |
| `encrypt_key` | Lark 控制台的 Encrypt Key（用于 AES-256-CBC） | 无 |
| `verification_token` | Lark 控制台的 Verification Token | 无 |

### 加密（可选）

如果你在 Lark 控制台（Events & Callbacks > Encryption Strategy）配置了 Encrypt Key，需将其添加到配置中：

```json
{
  "type": "lark",
  "settings": {
    "app_id_env": "LARK_APP_ID",
    "app_secret_env": "LARK_APP_SECRET",
    "region": "global",
    "mode": "webhook",
    "webhook_port": 9321,
    "encrypt_key": "your-encrypt-key-here",
    "verification_token": "your-verification-token"
  }
}
```

启用加密后，Lark 会发送加密的 POST 请求体。网关使用 SHA-256 密钥派生的 AES-256-CBC 进行解密，并通过 `X-Lark-Signature` 头验证签名。

### 支持的消息类型

**入站消息：** 文本、图片、文件（PDF、文档）、音频、视频、表情包

**出站消息：** Markdown（通过互动卡片）、图片上传、文件上传

### 运行

```bash
# 启动 ngrok 隧道
ngrok http 9321

# 启动网关
LARK_APP_ID="cli_xxxxx" LARK_APP_SECRET="xxxxx" octos gateway --cwd /path/to/workdir
```

### 常见问题排查

| 问题 | 解决方案 |
|-------|----------|
| WS 端点返回 404 | Lark 国际版不支持 WebSocket，请使用 `"mode": "webhook"` |
| Challenge 验证失败 | 确认 ngrok 正在运行且 URL 与 Lark 控制台中的一致 |
| 收不到事件 | 添加事件后需发布应用版本；检查控制台中的 Event Log |
| 机器人不回复 | 确认已授予 `im:message:send_as_bot` 权限 |
| Ngrok URL 变了 | 免费 ngrok URL 每次重启都会变化，需在 Lark 控制台更新请求 URL |

---

## 邮件（IMAP/SMTP）

通过 IMAP 轮询收件箱获取入站消息，通过 SMTP 发送回复。需启用 `email` feature flag。

```bash
export EMAIL_USERNAME="bot@example.com"
export EMAIL_PASSWORD="app-specific-password"
```

```json
{
  "type": "email",
  "allowed_senders": ["trusted@example.com"],
  "settings": {
    "imap_host": "imap.gmail.com",
    "imap_port": 993,
    "smtp_host": "smtp.gmail.com",
    "smtp_port": 465,
    "username_env": "EMAIL_USERNAME",
    "password_env": "EMAIL_PASSWORD",
    "from_address": "bot@example.com",
    "poll_interval_secs": 30,
    "max_body_chars": 10000
  }
}
```

---

## 企业微信（WeCom）

需要一个配置了消息回调 URL 的自建应用。需启用 `wecom` feature flag。

```bash
export WECOM_CORP_ID="ww..."
export WECOM_AGENT_SECRET="..."
```

```json
{
  "type": "wecom",
  "settings": {
    "corp_id_env": "WECOM_CORP_ID",
    "agent_secret_env": "WECOM_AGENT_SECRET",
    "agent_id": "1000002",
    "verification_token": "...",
    "encoding_aes_key": "...",
    "webhook_port": 9322
  }
}
```

---

## 微信（通过 WorkBuddy 桥接）

普通微信用户可以通过 WorkBuddy 桌面端桥接连接到你的智能体。WorkBuddy 负责微信传输层；Octos 通过其 WeCom Bot 频道处理 AI 逻辑。

```
微信（手机） --> WorkBuddy（桌面端） --> 企业微信群机器人（WSS） --> octos wecom-bot 频道
```

### 配置步骤

1. 在[企业微信管理后台](https://work.weixin.qq.com/)的"应用管理 > 群机器人"中创建一个**企业微信群机器人**，记下 Bot ID 和 Secret。

2. 配置 `wecom-bot` 频道：

```bash
export WECOM_BOT_SECRET="your_robot_secret_here"
```

```json
{
  "type": "wecom-bot",
  "allowed_senders": [],
  "settings": {
    "bot_id": "YOUR_BOT_ID",
    "secret_env": "WECOM_BOT_SECRET"
  }
}
```

3. 构建并启动：

```bash
cargo build --release -p octos-cli --features "wecom-bot"
octos gateway
```

4. 安装 **WorkBuddy** 桌面客户端，通过扫码关联你的微信，并连接到同一个企业微信群机器人。

### 连接详情

| 属性 | 值 |
|----------|-------|
| 协议 | WebSocket (WSS) |
| 端点 | `wss://openws.work.weixin.qq.com` |
| 心跳 | 每 30 秒 Ping/pong |
| 自动重连 | 支持，指数退避（5s--60s） |
| 最大消息长度 | 4096 字符 |
| 消息格式 | Markdown |

`wecom-bot` 频道使用出站 WebSocket 连接——无需公网 URL 或端口转发。适合部署在 NAT 或防火墙后的服务器。

### 限制

- **仅支持文本** -- 语音和图片消息以占位符形式传递
- **不支持消息编辑** -- 回复以新消息形式发送
- **单向触发** -- 微信到 Octos 自动触发；主动推送需使用定时任务

---

## 会话控制命令

在任何网关频道中，以下命令用于管理对话会话：

| 命令 | 说明 |
|---------|-------------|
| `/new` | 创建新会话（从当前对话的最近 10 条消息分叉） |
| `/new <name>` | 创建命名会话 |
| `/s <name>` | 切换到指定名称的会话 |
| `/s` | 切换到默认会话 |
| `/sessions` | 列出当前聊天的所有会话 |
| `/back` | 切换到上一个活跃会话 |
| `/delete` | 删除当前会话 |

每个聊天同一时间只有一个**活跃**会话。消息会路由到活跃会话。非活跃会话仍可执行后台任务（深度搜索、流水线等）。当非活跃会话完成工作时，你会收到通知——使用 `/s <name>` 查看结果。

---

## 语音转写

来自频道的语音和音频消息在发送给智能体前会自动转写。系统优先尝试本地 ASR（通过 OminiX 引擎），本地不可用时降级到云端 Whisper。转写结果会以 `[transcription: ...]` 的形式前置。

```bash
# 本地 ASR（优先） -- 由 octos serve 自动设置
export OMINIX_API_URL="http://localhost:8080"

# 云端降级
export GROQ_API_KEY="gsk_..."
```

`config.json` 中的语音配置：

```json
{
  "voice": {
    "auto_asr": true,
    "auto_tts": true,
    "default_voice": "vivian",
    "asr_language": null
  }
}
```

- **`auto_asr`** -- 自动转写收到的语音/音频消息
- **`auto_tts`** -- 用户发送语音时自动合成语音回复
- **`default_voice`** -- 自动 TTS 的语音预设
- **`asr_language`** -- 强制指定转写语言（`null` = 自动检测）

---

## 访问控制

使用 `allowed_senders` 限制谁可以与智能体交互。空列表表示允许所有人。

```json
{
  "type": "telegram",
  "allowed_senders": ["123456", "789012"]
}
```

每种频道类型使用各自的发送者标识格式（Telegram 用户 ID、邮箱地址、企业微信用户 ID 等）。

---

## 定时任务

智能体可以调度周期性任务，通过任意频道发送消息：

```bash
octos cron list                          # 列出活跃任务
octos cron list --all                    # 包含已禁用的任务
octos cron add --name "report" --message "Generate daily report" --cron "0 0 9 * * * *"
octos cron add --name "check" --message "Check status" --every 3600
octos cron add --name "once" --message "Run migration" --at "2025-03-01T09:00:00Z"
octos cron remove <job-id>
octos cron enable <job-id>               # 启用任务
octos cron enable <job-id> --disable     # 禁用任务
```

任务支持可选的 `timezone` 字段，使用 IANA 时区名称（如 `"America/New_York"`、`"Asia/Shanghai"`）。未指定时使用 UTC。

---

## 消息合并

长回复会自动拆分为符合频道限制的分段：

| 频道 | 每条消息最大字符数 |
|---------|-----------------------|
| Telegram | 4000 |
| Discord | 1900 |
| Slack | 3900 |

拆分优先级：段落边界 > 换行符 > 句末 > 空格 > 硬截断。

---

## 配置热更新

网关会自动检测配置文件变更：

- **热更新**（无需重启）：系统提示词、AGENTS.md、SOUL.md、USER.md
- **需要重启**：服务商、模型、API 密钥、频道设置

变更通过 SHA-256 哈希检测，并附带防抖机制。
