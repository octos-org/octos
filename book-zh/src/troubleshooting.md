# 故障排查

本章按类别整理了常见问题及其解决方案，以及环境变量参考。

---

## API 与提供商问题

### API 密钥未设置

```
Error: ANTHROPIC_API_KEY environment variable not set
```

**解决方法**：在 shell 中导出密钥或通过 `octos status` 验证：

```bash
export ANTHROPIC_API_KEY="your-key"
```

如果以服务方式运行，确保环境变量设置在服务环境中（launchd plist 或 systemd 单元），而不仅仅是交互式 shell。

### 限流 (429)

重试机制会自动处理（3 次尝试，指数退避）。如果错误持续：
- 尝试通过 `/queue` 或聊天中的模型切换功能切换到其他提供商。
- 等待限流窗口重置。

### 调试日志

启用详细日志以诊断问题：

```bash
RUST_LOG=debug octos chat
RUST_LOG=octos_agent=trace octos chat --message "task"
```

---

## 构建问题

| 问题 | 解决方案 |
|---------|----------|
| Linux 上构建失败 | 安装构建依赖：`sudo apt install build-essential pkg-config libssl-dev` |
| macOS 代码签名警告 | 签名二进制文件：`codesign -s - ~/.cargo/bin/octos` |
| `octos: command not found` | 将 cargo bin 添加到 PATH：`export PATH="$HOME/.cargo/bin:$PATH"` |

---

## 频道特定问题

### Lark / 飞书

| 问题 | 解决方案 |
|-------|----------|
| WebSocket 端点返回 404 | Larksuite 国际版不支持 WebSocket 模式。在配置中使用 `"mode": "webhook"` |
| Challenge 验证失败 | 确保隧道（如 ngrok）正在运行且 URL 与飞书控制台中配置的一致 |
| 未收到事件 | 添加事件后需要发布应用版本。在控制台中检查事件日志检索 |
| 机器人不回复 | 检查是否已授予 `im:message:send_as_bot` 权限 |
| Markdown 未渲染 | 消息以交互卡片发送；飞书支持 Markdown 的一个子集 |
| 隧道 URL 变更 | 免费隧道 URL 在重启后会变化。在飞书控制台中更新请求 URL |

### 企业微信

**"Environment variable WECOM_BOT_SECRET not set"**

启动网关前设置密钥：

```bash
export WECOM_BOT_SECRET="your_secret"
```

**连接断开或无法订阅**

- 验证 `bot_id` 和密钥是否正确。
- 检查到 `wss://openws.work.weixin.qq.com` 的网络连通性。
- 频道最多自动重连 100 次（指数退避）。查看日志获取错误详情。

**消息未到达**

- 确认上游中继服务正在运行且已关联到你的账号。
- 检查企业微信群机器人是否与 octos 中配置的一致。
- 如果使用了 `allowed_senders`，验证发送者的企业微信用户 ID 是否在列表中。
- 检查重复消息过滤 -- 频道会对最近 1000 条消息 ID 去重。

**长消息被截断**

超过 4096 字符的消息会被 octos 自动拆分为多个分块。如果仍有截断，检查中继服务本身的消息长度设置。

---

## 平台特定问题

| 问题 | 解决方案 |
|---------|----------|
| 仪表板无法访问 | 检查端口：`octos serve --port 8080`，打开 `http://localhost:8080/admin/` |
| WSL2 端口未转发 | 重启 WSL：`wsl --shutdown` 然后重新打开终端 |
| 服务无法启动 | 查看日志：`tail -f ~/.octos/serve.log`（macOS）或 `journalctl --user -u octos-serve`（Linux） |
| Windows: 找不到 `octos` | 确保 `%USERPROFILE%\.cargo\bin` 在 PATH 中 |
| Windows: shell 命令失败 | 命令通过 `cmd /C` 执行；使用 Windows 兼容的语法 |

---

## 环境变量参考

| 变量 | 说明 |
|----------|-------------|
| `ANTHROPIC_API_KEY` | Anthropic API 密钥 |
| `OPENAI_API_KEY` | OpenAI API 密钥 |
| `GEMINI_API_KEY` | Gemini API 密钥 |
| `OPENROUTER_API_KEY` | OpenRouter API 密钥 |
| `DEEPSEEK_API_KEY` | DeepSeek API 密钥 |
| `GROQ_API_KEY` | Groq API 密钥 |
| `MOONSHOT_API_KEY` | Moonshot API 密钥 |
| `DASHSCOPE_API_KEY` | DashScope API 密钥 |
| `MINIMAX_API_KEY` | MiniMax API 密钥 |
| `ZHIPU_API_KEY` | 智谱 API 密钥 |
| `ZAI_API_KEY` | Z.AI API 密钥 |
| `NVIDIA_API_KEY` | Nvidia NIM API 密钥 |
| `OMINIX_API_URL` | 本地 ASR/TTS API 地址 |
| `RUST_LOG` | 日志级别（`error` / `warn` / `info` / `debug` / `trace`） |
| `TELEGRAM_BOT_TOKEN` | Telegram 机器人令牌 |
| `DISCORD_BOT_TOKEN` | Discord 机器人令牌 |
| `SLACK_BOT_TOKEN` | Slack 机器人令牌 |
| `SLACK_APP_TOKEN` | Slack 应用级令牌 |
| `FEISHU_APP_ID` | 飞书应用 ID |
| `FEISHU_APP_SECRET` | 飞书应用密钥 |
| `EMAIL_USERNAME` | 邮箱账户用户名 |
| `EMAIL_PASSWORD` | 邮箱账户密码 |
| `WECOM_CORP_ID` | 企业微信企业 ID |
| `WECOM_AGENT_SECRET` | 企业微信应用密钥 |
