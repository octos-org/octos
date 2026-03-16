# 通道集成对比：octos vs OpenClaw

octos 与 OpenClaw 消息平台集成方式的详细对比。

## 1. 核心抽象

### octos：扁平 Trait（12 个方法）

```rust
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;                                        // 通道标识
    async fn start(&self, inbound_tx: Sender<InboundMessage>);     // 启动监听
    async fn send(&self, msg: &OutboundMessage);                   // 发送消息
    fn is_allowed(&self, sender_id: &str) -> bool;                 // 权限检查
    fn max_message_length(&self) -> usize;                         // 消息长度限制
    async fn stop(&self);                                          // 优雅停止
    async fn send_typing(&self, chat_id: &str);                    // 输入指示器
    async fn send_listening(&self, chat_id: &str);                 // 语音录制指示器
    async fn send_with_id(&self, msg: &OutboundMessage) -> Option<String>;  // 发送并返回消息 ID
    async fn edit_message(&self, chat_id: &str, msg_id: &str, content: &str);  // 编辑消息
    async fn delete_message(&self, chat_id: &str, msg_id: &str);  // 删除消息
    async fn edit_message_with_metadata(&self, ...);               // 带元数据编辑
}
```

**优势**：简单、易于实现新通道（200-500 行）、单一二进制、编译时类型检查
**劣势**：无多账号、无富载荷抽象、无健康检查、无群组策略

### OpenClaw：插件适配器模式（约 20 个可选槽位）

每个通道由约 20 个可选适配器组成（config、outbound、security、gateway、streaming、threading、groups、actions、status 等）。

**优势**：丰富的能力声明、多账号原生、细粒度访问控制、内置健康监控
**劣势**：复杂（约 50 文件，约 15K 行）、需要 Node.js 运行时

## 2. 连接方式

| 平台 | octos | OpenClaw |
|------|---------|----------|
| **Telegram** | 长轮询（teloxide） | 长轮询（grammY） |
| **WhatsApp** | WebSocket 连接 Node.js 桥接（Baileys 辅助进程） | 直接嵌入 Baileys |
| **Discord** | Serenity 网关（WebSocket） | discord.js 网关 |
| **Slack** | Socket Mode（WebSocket） | Socket Mode |
| **飞书** | WebSocket（默认）或 Webhook（端口 9321） | 不支持 |
| **企业微信** | Webhook（端口 9322）+ REST API | 不支持 |
| **Twilio** | Webhook（端口 8090）+ REST API | 不支持 |
| **Signal** | 不支持 | signal-cli 关联设备 |
| **Matrix** | 不支持 | matrix-bot-sdk（扩展） |

### octos 独有：纯 Rust 密码学

octos 使用纯 Rust 实现密码学原语（SHA-1、SHA-256、AES-128-CBC、AES-256-CBC、HMAC、Base64），用于飞书、企业微信、Twilio 的 webhook 签名验证。无 OpenSSL 依赖，服务单一二进制部署模型。

### octos 独有：WhatsApp 桥接架构

```
WhatsApp ←→ Node.js 桥接（Baileys）←→ WebSocket ←→ octos
                                            ↓
                             端口 3001（WS）+ 端口 3002（媒体 HTTP）
```

## 3. 入站消息流水线对比

| 步骤 | octos | OpenClaw |
|------|---------|----------|
| **规范化** | 各通道内部处理 | 集中规范化层 |
| **访问控制** | `is_allowed()` 单次检查 | 3 级：私信+群组+操作 |
| **去重** | 仅飞书/Twilio（消息 ID 缓存，最大 1000） | 所有通道（可配置去抖窗口） |
| **消息合并** | 无——逐条处理 | 批量处理快速连续消息 |
| **提及剥离** | 仅 Slack | 所有通道（可配置剥离模式） |
| **队列策略** | 有界收件箱（32），背压通知 | 显式运行/排队/丢弃决策 |
| **线程上下文** | Slack thread_ts 在元数据中 | 所有通道的线程/回复感知 |

## 4. 出站消息流水线对比

| 步骤 | octos | OpenClaw |
|------|---------|----------|
| **崩溃恢复** | 无 | 预写式队列 |
| **发送钩子** | 无 | 预/后发送钩子 |
| **Markdown 转换** | 仅 Telegram | 所有平台 |
| **富载荷** | 通过 metadata JSON（仅 Telegram 内联键盘） | 专用 `sendPayload()` |
| **分块** | 段落感知，50 块限制 | 平台特定，Markdown 感知 |

### octos 消息分块（`coalesce.rs`）

断开优先级（从高到低）：
1. 段落分隔（`\n\n`）
2. 换行（`\n`）
3. 句子结束（`. `）
4. 单词空格
5. 硬字符边界（保留 UTF-8）

安全限制：最大 50 块，超出则截断并标记。

### OpenClaw Markdown 转换

- **Telegram**：Markdown → HTML（`<b>`、`<i>`、`<code>`）
- **WhatsApp**：`**粗体**` → `*粗体*`，`_斜体_` → `_斜体_`
- **Signal**：Markdown → 文本样式范围
- **Slack**：mrkdwn 格式
- **Discord**：原生 Markdown

octos 仅 Telegram 有 `markdown_to_telegram_html()` 转换，其他平台发送原始文本。

## 5. 各平台功能对比

### Telegram

| 功能 | octos | OpenClaw |
|------|---------|----------|
| 文本消息 | 有 | 有 |
| 图片/视频/语音 | 有（下载+发送） | 有 |
| 内联键盘 | 有（metadata JSON） | 有（channelData） |
| 回调查询 | 有（路由为消息） | 有 |
| 消息编辑/删除 | 有 | 有 |
| 输入指示器 | 有 | 有 |
| 语音录制指示器 | 有（ChatAction::RecordVoice） | 无 |
| 机器人命令 | 有（/new, /s, /sessions, /back, /delete） | 有 |
| 论坛主题 | 无 | 有 |
| Markdown → HTML | 有 | 有 |
| 重连退避 | 有（5s 基础，60s 最大） | 有 |

### WhatsApp

| 功能 | octos | OpenClaw |
|------|---------|----------|
| 协议 | Baileys（Node.js 桥接） | Baileys（直接集成） |
| 独立进程 | 是（bridge.js） | 否（同进程） |
| 输入指示器 | 有 | 有 |
| 二维码登录 | 有 | 有 |
| 投票 | 无 | 有 |
| Markdown 转换 | 无（原始文本） | 有 |

### 飞书（仅 octos）

| 功能 | 状态 |
|------|------|
| WebSocket 模式 | 有（默认） |
| Webhook 模式 | 有 |
| 中国/国际区域 | 有（cn/global URL） |
| 签名验证（SHA-1，纯 Rust） | 有 |
| AES-256-CBC 解密（纯 Rust） | 有 |
| 令牌刷新（7000s TTL） | 有 |
| 富卡片 | 有 |
| 消息去重 | 有 |

### 企业微信（仅 octos）

| 功能 | 状态 |
|------|------|
| Webhook 回调（端口 9322） | 有 |
| AES-128-CBC 加密（纯 Rust） | 有 |
| 令牌管理（自动刷新） | 有 |
| 文本/图片/语音/文件 | 有 |
| 部门定向（toparty） | 有 |

### Twilio（仅 octos）

| 功能 | 状态 |
|------|------|
| SMS/MMS | 有 |
| HMAC-SHA1 验证（纯 Rust） | 有 |
| 媒体发送/接收 | 有 |
| 最大消息长度 | 1600 字符 |

## 6. 访问控制对比

### octos：单级

```
网关配置
  → allowed_senders: ["user1", "user2"]（每通道条目）
  → Channel.is_allowed(sender_id) → bool

Telegram：复合 ID 匹配（"userId|username" → 匹配任一部分）
其他：直接 HashSet 查找
```

群组中没有提及门控——机器人回应**所有**消息。

### OpenClaw：三级

1. **私信策略**：`"allowlist"` / `"open"` / `"disabled"`
2. **群组策略**：提及门控、工具策略
3. **操作级别**：所有者/管理员检查

### 影响

octos 机器人在 Telegram 群组中会回应每条消息。这是显著的用户体验问题。提及门控是高优先级改进。

## 7. 媒体处理对比

| 方面 | octos | OpenClaw |
|------|---------|----------|
| 下载位置 | 每通道临时目录 | 可配置媒体根目录 |
| 大小限制 | 无（平台强制） | 每账号可配置 |
| 发送 API | 每通道各自实现 | 统一 `sendMedia()` |
| MIME 检测 | 基于扩展名 | Content-Type + 扩展名 |
| 清理 | 手动 | 自动清理策略 |

octos 共享工具（`media.rs`）：
```rust
download_media(client, url, headers, dest_dir, filename) → PathBuf
is_audio(path) → bool   // .ogg, .mp3, .m4a, .wav, .oga, .opus
is_image(path) → bool   // .jpg, .jpeg, .png, .gif, .webp
```

## 8. 错误处理对比

| 方面 | octos | OpenClaw |
|------|---------|----------|
| 崩溃恢复 | 无（消息丢失） | 预写式队列 |
| 重连退避 | Telegram 有（5s-60s） | 所有通道 |
| 断路器 | 无 | 级联故障保护 |
| 健康检查 | 无 | `probeAccount()` + `auditAccount()` |
| 状态问题收集 | 无 | `collectStatusIssues()` 含修复建议 |

## 9. 架构总结

```
                    octos                          OpenClaw
                    ──────                           ────────

抽象层          扁平 trait（12 方法）            插件适配器（约 20 槽位）
通道数          6 个内置（特性门控）             9 个内置 + 扩展
多账号          不支持                           一等公民
入站流水线      直接调度                         规范化→去重→合并
出站流水线      分块→发送                        WAL→钩子→格式化→发送
访问控制        1 级（允许名单）                 3 级（私信+群组+操作）
Markdown 转换   仅 Telegram                      所有平台
去重            仅飞书/Twilio                    所有通道
提及门控        无                               每群组可配置
密码学          纯 Rust（无 OpenSSL）            Node.js crypto
部署            单一二进制                       Node.js 进程
代码行数        约 5K（所有通道）                约 15K（通道系统）
中国平台        飞书+企业微信+Twilio             无
```

## 10. octos 改进建议

### 必须实现（高影响）

1. **群组提及门控** — 仅在 @提及或回复时响应。添加 `require_mention` 配置选项。
2. **通用消息去重** — 在网关调度器添加消息 ID 缓存（LRU，1000 条目，60s TTL）。
3. **WhatsApp Markdown 转换** — `**粗体**` → `*粗体*`，WhatsApp 有自己的格式语法。

### 应该实现（中等影响）

4. **通道健康检测** — Channel trait 添加 `health_check()` 方法，集成到管理仪表盘。
5. **富载荷透传** — 扩展 `OutboundMessage.metadata` 支持通道特定载荷。
6. **出站预写式队列** — 发送前持久化，崩溃恢复时重发。

### 可选实现（较低优先级）

7. **全平台 Markdown 转换** — Slack mrkdwn、Discord 原生、Signal 文本样式。
8. **每通道多账号** — 同类型多个机器人。
9. **发送钩子** — 消息修改和日志的预/后发送钩子。
