# OpenClaw 通道架构全面分析

对 OpenClaw 通道系统的深度分析，与 octos 的 `Channel` trait 进行对比，识别值得借鉴的模式和需要填补的差距。

## 1. 架构概览

OpenClaw 采用**基于插件的适配器模式**，每个通道由一组可选适配器组成。通道通过 `ChannelPlugin` 接口声明能力，包含约 20 个可选适配器槽位：

```
ChannelPlugin = {
  id, meta, capabilities, defaults,

  config:      ChannelConfigAdapter       // 多账号管理（必需）
  outbound:    ChannelOutboundAdapter     // 发送文本/媒体/投票
  security:    ChannelSecurityAdapter     // 私信/群组访问控制
  gateway:     ChannelGatewayAdapter      // 启动/停止监听
  messaging:   ChannelMessagingAdapter    // 目标解析
  mentions:    ChannelMentionAdapter      // @提及解析
  streaming:   ChannelStreamingAdapter    // 渐进式文本传输
  threading:   ChannelThreadingAdapter    // 回复/线程上下文
  groups:      ChannelGroupAdapter        // 群组特定策略
  directory:   ChannelDirectoryAdapter    // 列出对等节点/群组
  actions:     ChannelMessageActionAdapter // 代理可发现的操作
  status:      ChannelStatusAdapter       // 健康检测
  pairing:     ChannelPairingAdapter      // 二维码/代码链接
  heartbeat:   ChannelHeartbeatAdapter    // 存活检查
  agentPrompt: ChannelAgentPromptAdapter  // 每通道系统提示词补充
  agentTools:  ChannelAgentToolFactory    // 通道特定代理工具
  ...
}
```

octos 使用**扁平 trait**，包含 12 个方法。更简单，但扩展性较低。

## 2. 通道注册表

### OpenClaw：9 个内置 + 扩展

内置通道（`src/channels/registry.ts`）：
1. **telegram** — grammY 框架
2. **whatsapp** — Baileys（Web 客户端，二维码登录）
3. **discord** — discord.js
4. **irc** — 自定义 IRC 客户端
5. **googlechat** — Google Workspace API
6. **slack** — Socket Mode API
7. **signal** — signal-cli 关联设备
8. **imessage** — Apple Messages（开发中）
9. **line** — LINE Messaging API

扩展（插件系统）：
- **matrix** — `@vector-im/matrix-bot-sdk` + 端到端加密
- **mattermost**、**nextcloud-talk** 及 30+ 其他

### octos：6 个内置（特性门控）

1. **telegram** — teloxide
2. **whatsapp** — 自定义 WebSocket 桥接（Node.js 辅助进程）
3. **feishu** — 飞书 webhook + 事件 API
4. **twilio** — SMS（REST API）
5. **wecom** — 企业微信 webhook
6. **api** — HTTP/SSE 网关

### 差距分析

| 平台 | OpenClaw | octos | 备注 |
|------|----------|---------|------|
| Telegram | 有 | 有 | 两者都成熟 |
| WhatsApp | 有（Baileys） | 有（桥接） | octos 使用 Node 辅助进程 |
| Discord | 有 | 无 | octos 重要缺失 |
| Slack | 有 | 无 | octos 重要缺失 |
| Signal | 有 | 无 | |
| 飞书 | 无 | 有 | octos 优势 |
| 企业微信 | 无 | 有 | octos 优势 |
| Twilio/SMS | 无 | 有 | octos 优势 |
| Matrix | 扩展 | 无 | 建议添加 |
| IRC | 有 | 无 | 低优先级 |
| LINE | 有 | 无 | 地区性 |
| iMessage | 开发中 | 无 | 仅 Apple |

## 3. 多账号支持

### OpenClaw：一等公民

每种通道类型原生支持 N 个账号：

```json
{
  "channels": {
    "telegram": {
      "accounts": {
        "default": { "botToken": "..." },
        "alerts": { "botToken": "..." },
        "moderation": { "botToken": "..." }
      }
    }
  }
}
```

`ChannelConfigAdapter` 提供：
- `listAccountIds()` — 枚举账号
- `resolveAccount(accountId)` — 获取单个账号配置
- `isEnabled(account)` / `isConfigured(account)` — 生命周期
- `describeAccount(account)` — 完整状态快照

代理可通过工具参数指定目标账号。

### octos：每通道单账号

`config.json` 中每个通道条目就是一个账号。同类型多账号需要使用不同的 `channel_type` 或设置创建多个通道条目。无一等公民多账号抽象。

### 建议

多账号对分离关注点很有价值（如同一 Telegram 上的客服机器人和告警机器人）。考虑在 octos 通道配置和路由中添加 `account_id` 字段。

## 4. 消息流

### 入站流水线（OpenClaw）

```
平台事件
  → 规范化（平台特定格式 → 标准格式）
  → 访问控制（私信策略：允许名单/开放/禁用）
  → 去重（窗口内去重）
  → 消息合并（批量处理快速连续消息）
  → 上下文丰富（发送者身份、线程上下文）
  → 代理处理
```

关键中间件：
- **提及剥离**：从消息文本中移除机器人 @提及
- **媒体提取**：将图片/音频/视频规范化为标准 URL
- **线程检测**：提取回复目标和线程 ID

### 入站流水线（octos）

```
平台事件
  → Channel.start() 回调 → InboundMessage
  → Gateway 调度器（解析会话密钥、主题）
  → ActorRegistry.dispatch()
  → SessionActor 收件箱
  → Agent.process_message()
```

### 差距：octos 缺少

- **去重** — 无 webhook 重复投递去重
- **消息合并** — 快速连续消息逐条处理
- **提及剥离** — 机器人 @提及保留在消息文本中

## 5. 出站流水线

### OpenClaw

```
代理响应
  → 负载规范化（为纯文本通道剥离 HTML）
  → 预写式队列（崩溃恢复）
  → 消息发送钩子（可修改/取消）
  → 文本分块（每通道限制，段落感知）
  → 平台格式化（Markdown → 平台原生）
  → 通道发送（文本/媒体/富载荷）
  → 消息已发送钩子（记录对话）
  → 队列清理（成功后确认）
```

平台特定格式化：
- **Telegram**：Markdown → HTML（`<b>`、`<i>`、`<code>`）
- **WhatsApp**：Markdown → WhatsApp 格式（`**粗体**` → `*粗体*`）
- **Signal**：Markdown → 文本样式范围
- **Discord**：原生 Markdown（无需转换）
- **Slack**：Block Kit 中的 mrkdwn 格式

### octos

```
代理响应
  → session_actor 发送 OutboundMessage
  → ChannelManager 路由到 Channel
  → split_message()（段落/句子感知分块）
  → Channel.send()
```

### 差距：octos 缺少

- **预写式队列** — 出站消息无崩溃恢复
- **平台特定 Markdown 转换** — 发送原始文本
- **发送钩子** — 通道层无预/后发送钩子
- **富载荷支持** — 无通道特定格式（内联键盘、Block Kit）

## 6. 输入指示器

### OpenClaw

精密的生命周期管理：

```typescript
createTypingCallbacks({
  start: () => sendTyping(),       // 平台特定 API
  stop: () => stopTyping(),
  keepaliveIntervalMs: 3000,       // 每 3 秒重发
  maxConsecutiveFailures: 2,       // 断路器
  maxDurationMs: 60000,            // 安全 TTL（最长 60 秒）
})
```

特性：
- 保活循环（平台在 5-10 秒后会过期输入状态）
- 断路器（N 次失败后自动禁用）
- 安全 TTL（防止无限输入）
- 回复完成后清理停止

### octos

```rust
// 在 StatusIndicator 中：
channel.send_typing(&chat_id).await;  // 状态循环中每 5 秒
```

更简单但功能可用。无断路器或安全 TTL。

## 7. 消息编辑

### OpenClaw

属于 `ChannelMessageActionAdapter` 框架：

平台支持情况：
- Telegram：完整编辑，任何时间
- Discord：15 分钟内，仅文本
- Slack：线程内随时
- WhatsApp：15 分钟内（有限支持）
- Signal/iMessage：不支持编辑

### octos

```rust
channel.edit_message(chat_id, message_id, new_content).await
```

相同概念，更简单的 API。平台限制由通道实现处理。

## 8. 富格式与通道特定功能

### OpenClaw

每个通道可暴露独特功能：

**Telegram**：
- 内联键盘按钮（`payload.channelData.telegram.buttons`）
- 论坛主题线程
- 贴纸包
- HTML 格式化模式

**Discord**：
- Webhook 身份（每个代理自定义用户名/头像）
- 嵌入字段
- 服务器管理工具
- 线程管理

**Slack**：
- Block Kit 格式化
- 机器人身份（用户名 + 表情图标）
- 文件上传
- App Home 标签

### octos

目前无通道特定功能暴露。所有通道使用相同的 `send()` / `edit_message()` 接口。Telegram 内联键盘、飞书卡片等需要向 `Channel` trait 添加方法或使用元数据。

### 建议

添加可选的 `send_rich()` 或 `send_with_metadata()` 方法，接受通道特定的 JSON 载荷。保持核心 trait 简单，同时允许平台原生功能。

## 9. 访问控制

### OpenClaw：三级安全

1. **私信策略**（`security.resolveDmPolicy()`）：
   - `"allowlist"` — 仅列出的用户 ID（默认，最安全）
   - `"open"` — 接受所有私信
   - `"disabled"` — 拒绝所有私信

2. **群组策略**（`groups.resolveToolPolicy()`）：
   - 每群组工具启用控制
   - 提及门控（仅在 @提及时响应）

3. **操作级别门控**：
   - 每个操作的所有者/管理员检查
   - 平台特定（Telegram 所有者、Discord 公会管理员）

允许名单文件：`.octos/channels/{channel}/{account}/allow-from`

### octos：单级

```rust
channel.is_allowed(sender_id) -> bool
```

加上网关级别的 `allowed_users` 配置。

### 差距

octos 缺少群组级别策略和提及门控。目前机器人在 Telegram 群组中会回应每条消息。

## 10. 健康监控

### OpenClaw

`ChannelStatusAdapter` 提供：
- `probeAccount()` — 异步健康检查（机器人能否访问 API？）
- `auditAccount()` — 深度验证（权限、webhook 状态）
- `collectStatusIssues()` — 问题列表及建议修复
- `buildAccountSnapshot()` — 仪表盘显示的完整状态

### octos

无等价功能。通道故障以运行时日志错误呈现。

### 建议

向 `Channel` trait 添加 `health_check()` 方法供管理仪表盘使用。

## 11. 流式/渐进式传输

### OpenClaw

`ChannelStreamingAdapter` + delta 流：
- 150ms 节流的文本增量从 LLM → 通道消息编辑
- 工具执行卡片实时进度
- Web UI 中的阅读指示器（动画圆点）

### octos

刚实现：`ChannelStreamReporter` + `run_stream_forwarder()`：
- 1 秒节流的文本增量 → 通道消息编辑
- 内联工具状态（`⚙ shell...` → `✓ shell`）
- 与 StatusIndicator 协调

## 12. 值得借鉴的关键模式

### 高优先级

1. **消息去重** — Webhook 平台可能重复投递。添加带 TTL 的消息 ID 缓存。

2. **群组提及门控** — 在群聊中，仅在 @提及时响应。防止机器人回应每条消息。

3. **平台特定 Markdown 转换** — WhatsApp、Signal 等有不同的格式规则。目前 octos 发送原始文本。

4. **通道健康检测** — 为管理仪表盘暴露 `probe()` 方法。

### 中优先级

5. **每通道多账号** — 对分离机器人个性或用例有用。

6. **富载荷透传** — 允许代理通过元数据发送 Telegram 内联键盘、飞书卡片等。

7. **预写式出站队列** — 消息传递的崩溃恢复。

### 低优先级

8. **通道特定代理工具** — 让通道注入自己的工具（如 Discord 管理）。

9. **引导向导** — CLI 引导的通道设置。

10. **插件发现** — 从外部 crate 动态加载通道。

## 13. 架构对比总结

| 方面 | OpenClaw | octos |
|------|----------|---------|
| **模式** | 插件适配器（约 20 个槽位） | 扁平 trait（12 个方法） |
| **通道数** | 9 个内置 + 扩展 | 6 个特性门控 |
| **多账号** | 一等公民 | 不支持 |
| **入站流水线** | 规范化→去重→合并→丰富 | 直接调度 |
| **出站流水线** | WAL→钩子→分块→格式化→发送 | 分块→发送 |
| **访问控制** | 3 级（私信+群组+操作） | 1 级（允许名单） |
| **富格式** | 每通道适配器 | 原始文本 |
| **健康监控** | 探测+审计+问题 | 无 |
| **流式传输** | Delta 流+工具卡片 | 状态指示器+流转发器 |
| **线程** | 完整线程/回复上下文 | 会话主题 |
| **错误恢复** | 预写式队列+重试 | 仅 LLM 层重试 |
| **可扩展性** | 运行时插件加载 | 编译时特性 |
| **复杂度** | 高（约 50 文件，约 15K 行） | 低（约 10 文件，约 3K 行） |
| **语言** | TypeScript（Node.js） | Rust（单一二进制） |

octos 以可扩展性换取简洁性和运维便利性（单一二进制，无运行时依赖）。扁平 `Channel` trait 覆盖 90% 的用例。剩余 10%（富格式、多账号、群组策略）可以在不引入完整插件系统的情况下逐步添加。
