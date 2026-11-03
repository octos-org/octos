# 配置

## 配置文件位置

配置文件按以下顺序加载（找到第一个即生效）：

1. `.octos/config.json` -- 项目级配置
2. `~/.config/octos/config.json` -- 全局配置

## 基本配置

最简配置只需指定 LLM 供应商和模型：

```json
{
  "provider": "anthropic",
  "model": "claude-sonnet-4-20250514",
  "api_key_env": "ANTHROPIC_API_KEY"
}
```

## 网关配置

要将 Octos 作为多渠道守护进程运行，需添加 `gateway` 部分：

```json
{
  "provider": "anthropic",
  "model": "claude-sonnet-4-20250514",
  "gateway": {
    "channels": [
      {"type": "cli"},
      {"type": "telegram", "allowed_senders": ["123456789"]},
      {"type": "discord", "settings": {"token_env": "DISCORD_BOT_TOKEN"}},
      {"type": "slack", "settings": {"bot_token_env": "SLACK_BOT_TOKEN", "app_token_env": "SLACK_APP_TOKEN"}},
      {"type": "whatsapp", "settings": {"bridge_url": "ws://localhost:3001"}},
      {"type": "feishu", "settings": {"app_id_env": "FEISHU_APP_ID", "app_secret_env": "FEISHU_APP_SECRET"}}
    ],
    "max_history": 50,
    "system_prompt": "You are a helpful assistant."
  }
}
```

## 环境变量展开

可在配置值中使用 `${VAR_NAME}` 语法引用环境变量：

```json
{
  "base_url": "${ANTHROPIC_BASE_URL}",
  "model": "${OCTOS_MODEL}"
}
```

## 完整配置参考

以下是包含所有可用字段的完整配置结构：

```json
{
  "version": 1,

  // LLM 供应商
  "provider": "anthropic",
  "model": "claude-sonnet-4-20250514",
  "base_url": null,
  "api_key_env": null,
  "api_type": null,

  // 回退链
  "fallback_models": [
    {
      "provider": "deepseek",
      "model": "deepseek-chat",
      "base_url": null,
      "api_key_env": "DEEPSEEK_API_KEY"
    }
  ],

  // 自适应路由
  "adaptive_routing": {
    "enabled": false,
    "latency_threshold_ms": 10000,
    "error_rate_threshold": 0.3,
    "probe_probability": 0.1,
    "probe_interval_secs": 60,
    "failure_threshold": 3
  },

  // 网关
  "gateway": {
    "channels": [{"type": "cli"}],
    "max_history": 50,
    "system_prompt": null,
    "queue_mode": "followup",
    "max_sessions": 1000,
    "max_concurrent_sessions": 10,
    "llm_timeout_secs": null,
    "llm_connect_timeout_secs": null,
    "tool_timeout_secs": null,
    "session_timeout_secs": null,
    "browser_timeout_secs": null
  },

  // 工具策略
  "tool_policy": {"allow": [], "deny": []},
  "tool_policy_by_provider": {},
  "context_filter": [],

  // 子供应商（用于 spawn 工具）
  "sub_providers": [
    {
      "key": "cheap",
      "provider": "deepseek",
      "model": "deepseek-chat",
      "description": "Fast model for simple tasks"
    },
    // Z.AI GLM 车道 —— api_key_env 省略时默认使用 ZAI_API_KEY
    {
      "key": "zai",
      "provider": "zai",
      "model": "glm-5.2",
      "description": "Z.AI GLM 5.2 lane (1M context)"
    }
  ],

  // 智能体设置
  "max_iterations": 50,

  // 向量嵌入（用于记忆的向量搜索）。省略本节时默认使用内置的进程内嵌入器
  // （llama.cpp，特性 embed-llama，默认启用）：首次使用时会把
  // EmbeddingGemma-300M Q8_0（334 MB，Gemma 使用条款——见
  // docs/THIRD_PARTY_MODELS.md）一次性下载到 <data_dir>/models/，无需 API 密钥。
  // 构建时加入 embed-llama-metal / -cuda 可将计算卸载到 GPU；否则在 CPU 上
  // 运行。model_path 可指向其他任意 GGUF；auto_download: false（或
  // OCTOS_NO_MODEL_DOWNLOAD=1）在提供模型之前让记忆保持纯关键词模式。
  // 切换 provider 或模型会改变向量维度，使已建索引失效——切换后需
  // 重新生成嵌入。
  "embedding": {
    "provider": "openai",
    "api_key_env": "OPENAI_API_KEY",
    "base_url": null
  },

  // 语音
  "voice": {
    "auto_asr": true,
    "auto_tts": false,
    "default_voice": "vivian",
    "asr_language": null
  },

  // 钩子
  "hooks": [],

  // MCP 服务器
  "mcp_servers": [],

  // 沙箱
  "sandbox": {
    "enabled": true,
    "mode": "auto",
    "allow_network": false
  },

  // 邮件（用于邮件渠道）
  "email": null,

  // 记忆注入 + 自动刷新（见「记忆与技能」）
  "memory": {
    "max_inject_tokens": 2500,
    "refresh": {
      "enabled": true,          // 三态：缺省 = 开启（默认）
      "extract_model": null,    // null = 使用 profile 服务商
      "consolidate_model": null,
      "max_extractions_per_day": 20,
      "max_daily_tokens": 200000,
      "consolidate_interval_minutes": 30
    }
  },

  // 浏览器 AppUI 访问（仅 serve 模式）
  "appui": {
    "allowed_origins": ["https://octos.example.com"]
  },

  // 仪表板认证（仅 serve 模式）
  "dashboard_auth": null,

  // 监控（仅 serve 模式）
  "monitor": null
}
```

> `memory.refresh` 流水线**默认开启**。完整字段列表与 `octos memory` 命令见[记忆与技能 → 自动记忆刷新](./memory-skills.md)。通过 `"enabled": false` 或 `OCTOS_MEMORY_REFRESH_ENABLED=0` 退出。

## 浏览器 AppUI Origin

`octos serve` 会保留既有 OminiX/base-domain 与开发 Origin，并自动允许
实际绑定端口（包括系统为 `--port 0` 选定的端口）上的 loopback Origin。
其他浏览器 Origin 都必须逐个
显式配置——包括用 LAN 地址或自定义主机名访问同一份内嵌资源的情况：

```json
{
  "appui": {
    "allowed_origins": [
      "https://octos.example.com",
      "https://octos.lan.example:50081"
    ]
  }
}
```

每项只能包含 `http://` 或 `https://`、主机以及可选端口。userinfo、路径、
query、fragment、通配符、`null` 或其他 scheme 会让启动直接失败。非空的
`OCTOS_APPUI_ALLOWED_ORIGINS` 使用逗号分隔，并整体覆盖配置文件列表；
空值不覆盖配置。

反向代理必须显式填写浏览器实际访问的公开 Origin。Octos 不会从 `Host`
或 `X-Forwarded-*` 推导信任，也不会猜测 LAN 地址。非 loopback/生产
Origin 应使用 HTTPS。浏览器认证 token 按 Origin 存储；URL 变化后请
打开公开 Origin 并在该页面重新登录，在 localhost 登录不会自动认证
另一个公开 Origin。

## 运行时工具配置

`octos chat` 和 `octos acp` 在启动时解析运行时 profile，用于控制工具授权和
agent 设置。模型请求使用固定的常用工具与调用入口；其他工具的 schema
通过发现操作返回，不再全部附加到每次请求中。

内置配置：

| 配置 | 工具访问 |
|------|----------|
| `coding`（默认） | 四个常用工具加 `discover_tools` / `run_tool`；所有已注册且获授权的其他能力可通过发现操作访问。 |
| `swarm` | 相同的固定接口，并为协作工作流设置显式允许列表。 |

直接运行 `octos chat` 即使用标准接口。自定义 profile 仍然配置权限、模型
偏好和 agent 设置。旧的工具展示变体及其开关已移除；访问其他获授权工具
无需选择特殊 profile。

项目配置可写入 `~/.octos/profiles/<name>/profile.json`，或通过 `--profile`
传入路径。允许列表支持 `group:<id>`、精确名称和 `prefix*` 通配符：

```json
{
  "name": "coding-project",
  "version": 1,
  "tools": {
    "mode": "allow_list",
    "tools": ["group:fs", "group:runtime", "group:search", "group:memory",
              "spawn", "ask_user_question", "group:web"]
  },
  "agents": ["research-worker", "repo-editor"]
}
```

注意：

- profile 筛选在插件、技能和 MCP 注册之后运行，对这些工具与内置工具
  同样生效。标记为 `spawn_only` 的工具不由此筛选移除；`run_pipeline`
  在注册时根据 profile 允许列表控制。
- `spawn` worker 根据自身的有效 profile 和权限建立工具注册表，也使用
  标准固定调用接口。
- `config.json` 中的 `tool_policy` 仍然生效，可进一步收窄权限；拒绝优先。
- `octos gateway` / `octos serve` 同样使用标准调用接口；host 和会话限制
  继续决定哪些能力可用。

## 人工审批规则

命中规则的工具调用会暂停当前轮次，直到授权的人在渠道上批准或拒绝（Matrix 优先；Robrix 等能力客户端渲染原生的 批准/拒绝 按钮，其他客户端显示文本回退）：

```json
{
  "approval_policy": {
    "default": "allow",
    "rules": [{
      "tools": ["shell", "write_file"],
      "require_approval": true,
      "risk_level": "critical",
      "authorized_approvers": ["@alice:example.org"],
      "expires_in_secs": 600,
      "on_timeout": "notify"
    }]
  }
}
```

- 规则按工具名精确匹配；第一条命中的规则生效。
- 审批绑定到精确的工具参数（SHA-256 摘要）、发起房间以及 `authorized_approvers` 列表；每个请求只能被消费一次。
- `expires_in_secs` 限定请求可应答的时长；到期后聊天会收到通知（`on_timeout: "notify"`）。
- 待处理审批保存在内存中：网关重启会丢弃它们（审批卡片仍留在聊天中，但应答会报告该请求未知）。
- 决策会追加到 `<data_dir>/audit/` 下的 JSONL 审计日志（`OCTOS_APPROVALS_AUDIT_*` 环境变量控制轮转/保留）。
- 也可通过 `profile.config.approval_policy` 按 profile 配置。

## 环境变量

### LLM 供应商

| 变量 | 说明 |
|------|------|
| `ANTHROPIC_API_KEY` | Anthropic (Claude) API 密钥 |
| `OPENAI_API_KEY` | OpenAI API 密钥 |
| `GEMINI_API_KEY` | Google Gemini API 密钥 |
| `OPENROUTER_API_KEY` | OpenRouter API 密钥 |
| `DEEPSEEK_API_KEY` | DeepSeek API 密钥 |
| `GROQ_API_KEY` | Groq API 密钥 |
| `MOONSHOT_API_KEY` | Moonshot/Kimi API 密钥 |
| `DASHSCOPE_API_KEY` | 阿里云 DashScope (Qwen) API 密钥 |
| `MINIMAX_API_KEY` | MiniMax API 密钥 |
| `ZHIPU_API_KEY` | 智谱 (GLM) API 密钥 |
| `ZAI_API_KEY` | Z.AI API 密钥 |
| `NVIDIA_API_KEY` | Nvidia NIM API 密钥 |

### 搜索

| 变量 | 说明 |
|------|------|
| `BRAVE_API_KEY` | Brave Search API 密钥 |
| `PERPLEXITY_API_KEY` | Perplexity Sonar API 密钥 |
| `YDC_API_KEY` | You.com API 密钥 |

### 渠道

| 变量 | 说明 |
|------|------|
| `TELEGRAM_BOT_TOKEN` | Telegram 机器人令牌 |
| `DISCORD_BOT_TOKEN` | Discord 机器人令牌 |
| `SLACK_BOT_TOKEN` | Slack 机器人令牌 |
| `SLACK_APP_TOKEN` | Slack 应用级令牌 |
| `FEISHU_APP_ID` | 飞书/Lark 应用 ID |
| `FEISHU_APP_SECRET` | 飞书/Lark 应用密钥 |
| `WECOM_CORP_ID` | 企业微信企业 ID |
| `WECOM_AGENT_SECRET` | 企业微信应用密钥 |
| `EMAIL_USERNAME` | 邮箱账户用户名 |
| `EMAIL_PASSWORD` | 邮箱账户密码 |

### 邮件（send-email 技能）

| 变量 | 说明 |
|------|------|
| `SMTP_HOST` | SMTP 服务器主机名 |
| `SMTP_PORT` | SMTP 服务器端口 |
| `SMTP_USERNAME` | SMTP 用户名 |
| `SMTP_PASSWORD` | SMTP 密码 |
| `SMTP_FROM` | SMTP 发件人地址 |
| `LARK_APP_ID` | 飞书邮箱应用 ID |
| `LARK_APP_SECRET` | 飞书邮箱应用密钥 |
| `LARK_FROM_ADDRESS` | 飞书邮箱发件人地址 |

### 语音

| 变量 | 说明 |
|------|------|
| `OMINIX_API_URL` | OminiX ASR/TTS API 地址 |

### 会话存储

| 变量 | 说明 |
|------|------|
| `OCTOS_SESSION_SEGMENT_BYTES` | 活跃会话文件封存为分段的大小（默认 8 MiB） |
| `OCTOS_SESSION_LOAD_BUDGET_BYTES` | 普通加载按新到旧读取的会话历史字节数（默认 32 MiB；`0` = 不限） |

### 系统

| 变量 | 说明 |
|------|------|
| `RUST_LOG` | 日志级别（error/warn/info/debug/trace） |
| `OCTOS_LOG_JSON` | 启用 JSON 格式日志（设置为任意值即可） |
| `OCTOS_APPUI_ALLOWED_ORIGINS` | 非空、逗号分隔的精确浏览器 Origin；覆盖 `appui.allowed_origins` |

## 文件目录结构

```
~/.octos/                        # 全局配置目录
├── auth.json                   # 已存储的 API 凭据（权限 0600）
├── profiles/                   # Profile 配置（serve 模式）
│   ├── my-bot.json
│   └── work-bot.json
├── skills/                     # 全局自定义技能
└── serve.log                   # serve 模式日志文件

.octos/                          # 项目/Profile 数据目录
├── config.json                 # 配置文件
├── cron.json                   # 定时任务
├── AGENTS.md                   # 智能体指令
├── SOUL.md                     # 个性定义
├── USER.md                     # 用户信息
├── HEARTBEAT.md                # 后台任务
├── sessions/                   # 对话历史（JSONL）
├── memory/                     # 记忆文件
│   ├── MEMORY.md               # 长期记忆
│   └── 2025-02-10.md           # 每日记忆
├── skills/                     # 自定义技能
├── episodes.redb               # 情景记忆数据库
└── history/
    └── chat_history            # Readline 历史记录
```
