# Matrix 桥接中枢方案分析

## 摘要

本文档评估使用 Matrix 服务器作为 crew-rs 的统一消息桥接中枢，让所有外部消息平台（Telegram、WhatsApp、Discord 等）通过 Matrix 桥接连接到 crew-rs，而非直接 API 集成。

**结论**：Matrix 中枢方案不建议替代 crew-rs 现有的直接集成，但作为**可选附加通道**很有价值——适用于小众平台和已有 Matrix 基础设施的用户。

## 背景

### crew-rs 现有架构（直接集成）

```
Telegram ──→ TelegramChannel  ──→┐
WhatsApp ──→ WhatsAppChannel  ──→┤
飞书     ──→ FeishuChannel    ──→┤──→ Gateway ──→ SessionActor ──→ Agent
Twilio   ──→ TwilioChannel    ──→┤
企业微信 ──→ WeComChannel     ──→┘
```

每个平台都有专用的 Rust 通道适配器，实现 `Channel` trait，完整访问平台原生 API（内联键盘、消息编辑、输入指示器、富媒体卡片）。

### 提议的 Matrix 中枢架构

```
Telegram ──→ mautrix-telegram ──→┐
WhatsApp ──→ mautrix-whatsapp ──→┤
Discord  ──→ mautrix-discord  ──→┤──→ Synapse ──→ MatrixChannel ──→ Gateway
Signal   ──→ mautrix-signal   ──→┤
Slack    ──→ mautrix-slack    ──→┘
```

crew-rs 只需实现一个 Matrix 通道适配器。所有外部平台通过 Matrix 桥接（mautrix 系列）连接。每个外部用户表现为幽灵用户（如 `@telegram_12345:server`）。

## Matrix 桥接生态

### 生产级桥接（mautrix 系列）

| 平台 | 桥接 | 状态 | 备注 |
|------|------|------|------|
| Telegram | mautrix-telegram | 稳定 | 功能完整，维护活跃 |
| WhatsApp | mautrix-whatsapp | 稳定 | 多设备 API，可能周期性断连 |
| Discord | mautrix-discord | 稳定 | 较新，比 matrix-appservice-discord 更完整 |
| Signal | mautrix-signal | 稳定 | 支持历史消息回填 |
| Slack | mautrix-slack | 稳定 | 工作区级集成 |
| Facebook Messenger | mautrix-meta | 稳定 | 与 Instagram 共享代码库 |
| Instagram | mautrix-meta | 稳定 | 仅私信 |
| Google Chat | mautrix-googlechat | 可用 | 维护较少 |
| iMessage | mautrix-imessage | 可用 | 需要 macOS 主机 |
| IRC | mautrix-irc (bridgev2) | 稳定 | 传统协议 |

### 实验性/社区桥接

| 平台 | 状态 | 备注 |
|------|------|------|
| 微信 | 实验性 | 两个实现，均未达生产级 |
| LINE | 实验性 | 社区维护 |
| KakaoTalk | 实验性 | 社区维护 |

mautrix 桥接由 Tulir Asokan 维护，经 Beeper（2024 年被 Automattic 收购）大规模商业验证。

## 桥接工作原理

### 消息流

1. 外部用户在其平台发送消息（如 Telegram）
2. 桥接进程（mautrix-telegram）通过平台 API 接收
3. 桥接在 Matrix 上创建"幽灵用户"（如 `@telegram_12345:yourserver.com`）
4. 桥接将消息转发到 Matrix 服务器上的"门户房间"
5. crew-rs 的 Matrix 客户端通过 `/sync` API 接收消息
6. crew-rs 处理后在 Matrix 房间回复
7. 桥接接收回复并发送回原平台

### 识别来源平台

幽灵用户 MXID 编码了平台信息：
- `@telegram_{userid}:server` — Telegram
- `@whatsapp_{phone}:server` — WhatsApp
- `@discord_{userid}:server` — Discord
- `@signal_{uuid}:server` — Signal
- `@slack_{userid}:server` — Slack

### 桥接保留的功能

- 文本消息（所有平台）
- 图片、视频、文件、语音消息（大多数平台）
- 表情反应（大多数平台双向）
- 消息编辑（Telegram、Discord、Slack）
- 回复/引用（大多数平台）
- 输入指示器（需 MSC2409）

### 桥接丢失的功能

- **Telegram 内联键盘/机器人按钮** — 被扁平化或丢弃
- **平台特定富媒体卡片** — Slack Block Kit、Discord 嵌入、飞书交互卡片
- **机器人专属 API** — Telegram Bot API 命令、Discord 斜杠命令
- **语音/视频通话** — 不桥接
- **WhatsApp 商业模板** — 不可用
- **平台原生格式细节** — 简化为基础 HTML/Markdown

## 对比

| 因素 | Matrix 中枢 | 直接集成（当前方案） |
|------|------------|-------------------|
| **支持平台数** | 10+ 仅需配置 | 每个需要 Rust 适配器 |
| **新增平台** | 部署 Docker 容器 | 编写 Rust 代码（数小时到数天） |
| **内联键盘/按钮** | 丢失 | 完整原生支持 |
| **语音消息** | 基础桥接 | 完整原生支持 |
| **输入指示器** | 桥接（有延迟） | 直接，即时 |
| **消息编辑** | 支持（大多数平台） | 完整原生支持 |
| **延迟** | 每跳 +50-200ms | 直连 |
| **运维复杂度** | Synapse + Postgres + N 个桥接（6+ 服务） | 单一二进制 |
| **可靠性** | 更多故障点；WhatsApp/Signal 周期性断连 | 直连，更少移动部件 |
| **机器人特定功能** | 通过桥接不可见 | 完全访问 |
| **资源占用** | Synapse + 桥接约 2 GB RAM | 包含在 crew 二进制中 |

## 基础设施需求

### 自托管 Matrix 部署

**服务器**：Synapse（Python，85% 市场份额，最佳桥接兼容性）
- 替代方案：Dendrite（Go，维护模式）、Conduit（Rust，测试版）

**数据库**：PostgreSQL（Synapse 生产环境必需）

**资源估算**（单用户 AI 代理中枢）：
- 内存：约 1-2 GB（Synapse 约 500 MB，每个桥接约 100 MB）
- 磁盘：约 10 GB
- CPU：低流量场景最小化

### Docker Compose 参考

```yaml
services:
  synapse:
    image: matrixdotorg/synapse:latest
    volumes: ["./synapse-data:/data"]
    ports: ["8008:8008"]

  mautrix-telegram:
    image: dock.mau.dev/mautrix/telegram:latest
    volumes: ["./mautrix-telegram:/data"]

  mautrix-whatsapp:
    image: dock.mau.dev/mautrix/whatsapp:latest
    volumes: ["./mautrix-whatsapp:/data"]

  mautrix-discord:
    image: dock.mau.dev/mautrix/discord:latest
    volumes: ["./mautrix-discord:/data"]

  mautrix-signal:
    image: dock.mau.dev/mautrix/signal:latest
    volumes: ["./mautrix-signal:/data"]

  postgres:
    image: postgres:15
    volumes: ["./postgres-data:/var/lib/postgresql/data"]
    environment:
      POSTGRES_PASSWORD: synapse
```

每个桥接生成注册 YAML 文件，需添加到 Synapse 的 `homeserver.yaml` 中的 `app_service_config_files`。

## 建议

### 不要替换现有直接集成

crew-rs 已有 Telegram、WhatsApp、飞书、Twilio、企业微信的完整适配器。切换到 Matrix 桥接会：

1. **丢失平台特定功能**（内联键盘、富媒体卡片、机器人 API）
2. **增加运维复杂度**（从单一二进制变为 6+ 服务）
3. **增加延迟**（每条消息 +50-200ms）
4. **降低可靠性**（更多故障点，桥接断连）

### 应当添加 Matrix 作为可选通道

为 crew-rs 实现 `MatrixChannel` 适配器提供两个好处：

1. **已有 Matrix 基础设施的用户**可通过其服务器连接 crew——已运行的桥接直接可用
2. **小众平台**（LINE、KakaoTalk、Google Chat、IRC）可通过社区桥接访问，无需编写 Rust 代码

### 实现方案

使用 `matrix-sdk` Rust crate 添加 `MatrixChannel`，实现 `Channel` trait：
- 连接可配置的服务器，使用访问令牌认证
- 将 Matrix 房间映射到会话（房间 ID → 会话密钥）
- 解析幽灵用户 MXID 识别来源平台
- 支持文本、媒体、反应、消息编辑
- 可选 E2EE（通过 `matrix-sdk-crypto`）

配置示例（`config.json`）：
```json
{
  "channels": [{
    "channel_type": "matrix",
    "settings": {
      "homeserver": "https://matrix.example.com",
      "user_id": "@crew:example.com",
      "access_token": "syt_...",
      "allowed_rooms": ["!roomid:example.com"]
    }
  }]
}
```

## OpenClaw 参考

OpenClaw 在 `extensions/matrix/` 中实现 Matrix 作为独立通道，使用 `@vector-im/matrix-bot-sdk`（Node.js）。它**并未**将 Matrix 用作桥接中枢——每个通道（Telegram、Discord、Slack、Matrix 等）都有独立的适配器。Matrix 是对等通道，不是中央路由器。

## 延伸阅读

- [Matrix.org 桥接](https://matrix.org/ecosystem/bridges/)
- [mautrix 桥接文档](https://docs.mau.fi/bridges/)
- [matrix-rust-sdk](https://github.com/matrix-org/matrix-rust-sdk)
- [Synapse 文档](https://element-hq.github.io/synapse/latest/)
- [mautrix bridgev2 框架](https://docs.mau.fi/bridges/general/bridgev2/)
