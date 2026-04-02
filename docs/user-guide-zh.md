# Octos 用户指南

部署、配置和使用 Octos AI 智能体平台的完整指南。

---

## 目录

1. [概览](#1-概览)
2. [仪表盘与 OTP 登录](#2-仪表盘与-otp-登录)
3. [配置 LLM 提供商](#3-配置-llm-提供商)
4. [故障转移与自适应路由](#4-故障转移与自适应路由)
5. [搜索 API 配置](#5-搜索-api-配置)
6. [工具配置](#6-工具配置)
7. [工具策略](#7-工具策略)
8. [配置文件管理](#8-配置文件管理)
9. [子账户管理](#9-子账户管理)
10. [聊天中切换模型](#10-聊天中切换模型)
11. [聊天功能与命令](#11-聊天功能与命令)
12. [内置应用技能](#12-内置应用技能)
    - [新闻获取](#121-新闻获取)
    - [深度搜索](#122-深度搜索)
    - [深度爬取](#123-深度爬取)
    - [发送邮件](#124-发送邮件)
    - [账户管理器](#125-账户管理器)
    - [时钟](#126-时钟)
    - [天气](#127-天气)
13. [平台技能 (ASR/TTS)](#13-平台技能-asrtts)
14. [自定义技能安装](#14-自定义技能安装)
15. [配置参考](#15-配置参考)
16. [Matrix Appservice（Palpo）](#16-matrix-appservicepalpo)

---

## 1. 概览

Octos 是一个 Rust 原生的 AI 智能体平台，支持以下运行模式：

- **`octos serve`** — 控制面板 + 管理仪表盘。管理多个 **配置文件**（机器人实例），每个实例作为独立的 gateway 子进程运行，拥有独立的配置、记忆、会话和消息通道。
- **`octos gateway`** — 单个 gateway 实例，服务于各消息通道（Telegram、Discord、Slack、WhatsApp、飞书、邮件、企业微信、Matrix）。
- **`octos chat`** — 交互式 CLI 聊天，用于开发和测试。

### 架构

```
octos serve（控制面板 + 仪表盘）
  ├── 配置 A → gateway 进程（Telegram、WhatsApp）
  ├── 配置 B → gateway 进程（飞书、Slack）
  └── 配置 C → gateway 进程（CLI）
       │
       ├── LLM 提供商（kimi-2.5、deepseek-chat、gpt-4o 等）
       ├── 工具注册表（shell、文件、搜索、网页、技能...）
       ├── 会话存储（每通道对话历史）
       ├── 记忆系统（MEMORY.md、每日笔记、回忆录）
       └── 技能（内置 + 自定义）
```

每个配置文件完全隔离 — 拥有独立的数据目录、记忆、会话、技能和 API 密钥。可以在配置文件下创建子账户，子账户继承父配置的 LLM 设置。

---

## 2. 仪表盘与 OTP 登录

管理仪表盘是嵌入在 `octos serve` 二进制文件中的 React Web 应用。它提供了管理配置文件、监控 gateway 状态和配置系统的可视化界面。

### 2.1 访问仪表盘

```bash
# 启动控制面板
octos serve --host 0.0.0.0 --port 3000

# 仪表盘地址：
# http://localhost:3000
```

如果在反向代理（如 Caddy 或 Nginx）后运行，请配置转发到 serve 端口。

### 2.2 OTP 邮件认证

仪表盘使用基于邮件的一次性密码（OTP）认证。不存储密码 — 每次登录时向用户发送 6 位验证码。

#### 配置 SMTP 发送 OTP 邮件

在 serve 配置文件中添加 `dashboard_auth`（`~/.octos/config.json` 或按配置文件）：

```json
{
  "dashboard_auth": {
    "smtp": {
      "host": "smtp.gmail.com",
      "port": 465,
      "username": "your-email@gmail.com",
      "password_env": "SMTP_PASSWORD",
      "from_address": "your-email@gmail.com"
    },
    "session_expiry_hours": 24,
    "allow_self_registration": false
  }
}
```

- **`host`** — SMTP 服务器（如 `smtp.gmail.com`、`smtp.office365.com`）
- **`port`** — 465 为隐式 TLS，587 为 STARTTLS
- **`username`** — SMTP 登录用户名
- **`password_env`** — 存放 SMTP 密码的环境变量名（如 `SMTP_PASSWORD`）。Gmail 请使用[应用密码](https://support.google.com/accounts/answer/185833)
- **`from_address`** — OTP 邮件的发件人地址
- **`session_expiry_hours`** — 登录会话有效期（默认：24 小时）
- **`allow_self_registration`** — 如果为 `false`，只有预先创建的用户才能登录

启动前设置 SMTP 密码环境变量：

```bash
export SMTP_PASSWORD="your-app-password"
```

#### 登录流程

1. 在浏览器中打开仪表盘
2. 在登录页面输入邮箱地址
3. 查收包含 6 位 OTP 验证码的邮件
4. 在验证页面输入验证码
5. 登录成功，在配置的会话时长内保持登录状态

**安全细节：**
- 每个邮箱每 60 秒只能请求一次 OTP（限流）
- OTP 在 5 分钟后过期
- 输错 3 次后 OTP 失效
- 会话令牌：64 字符十六进制字符串（32 字节随机数）
- 使用常量时间比较防止时序攻击
- 如果 `allow_self_registration` 禁用且邮箱未注册，不发送邮件（但服务器返回成功以防止邮箱枚举）

**开发模式：** 如果未配置 SMTP，OTP 验证码会打印到服务器控制台日志中而不是发送邮件。适用于本地开发。

### 2.3 仪表盘功能

登录后，仪表盘提供：

- **总览** — 配置文件总数、运行中/已停止数量、所有机器人的快速状态
- **配置管理** — 创建、编辑、启动、停止、重启和删除配置文件
- **日志查看** — 每个 gateway 进程的实时 SSE 日志流
- **提供商测试** — 在部署前测试 LLM 提供商/模型/API 密钥组合
- **WhatsApp 二维码** — 扫描二维码绑定 WhatsApp 号码
- **平台技能** — 监控和管理 OminiX ASR/TTS 服务
- **指标** — 每个配置文件的 LLM 提供商 QoS 指标（延迟、错误率）

---

## 3. 配置 LLM 提供商

Octos 开箱即用支持 14 个 LLM 提供商。每个提供商需要设置对应的环境变量 API 密钥。

### 3.1 支持的提供商

| 提供商 | 环境变量 | 默认模型 | API 格式 | 别名 |
|--------|----------|----------|----------|------|
| `anthropic` | `ANTHROPIC_API_KEY` | claude-sonnet-4-20250514 | 原生 Anthropic | — |
| `openai` | `OPENAI_API_KEY` | gpt-4o | 原生 OpenAI | — |
| `gemini` | `GEMINI_API_KEY` | gemini-2.0-flash | 原生 Gemini | — |
| `openrouter` | `OPENROUTER_API_KEY` | anthropic/claude-sonnet-4-20250514 | 原生 OpenRouter | — |
| `deepseek` | `DEEPSEEK_API_KEY` | deepseek-chat | OpenAI 兼容 | — |
| `groq` | `GROQ_API_KEY` | llama-3.3-70b-versatile | OpenAI 兼容 | — |
| `moonshot` | `MOONSHOT_API_KEY` | kimi-k2.5 | OpenAI 兼容 | `kimi` |
| `dashscope` | `DASHSCOPE_API_KEY` | qwen-max | OpenAI 兼容 | `qwen` |
| `minimax` | `MINIMAX_API_KEY` | MiniMax-Text-01 | OpenAI 兼容 | — |
| `zhipu` | `ZHIPU_API_KEY` | glm-4-plus | OpenAI 兼容 | `glm` |
| `zai` | `ZAI_API_KEY` | glm-5 | Anthropic 兼容 | `z.ai` |
| `nvidia` | `NVIDIA_API_KEY` | meta/llama-3.3-70b-instruct | OpenAI 兼容 | `nim` |
| `ollama` | *（无需）* | llama3.2 | OpenAI 兼容 | — |
| `vllm` | `VLLM_API_KEY` | *（必须指定）* | OpenAI 兼容 | — |

#### 如何获取 API 密钥

**Google Gemini：**
1. 访问 [Google AI Studio](https://aistudio.google.com/apikey)
2. 使用 Google 账号登录
3. 点击"Create API Key"，选择或创建一个 Google Cloud 项目
4. 复制生成的 API 密钥
5. 设置环境变量：`export GEMINI_API_KEY="your-key"`

**阿里云灵积 DashScope（通义千问 Qwen）：**
1. 访问[灵积控制台](https://dashscope.console.aliyun.com/)
2. 注册或登录阿里云账号
3. 进入 **API-KEY 管理** 页面
4. 点击"创建新的 API-KEY"
5. 复制生成的密钥
6. 设置环境变量：`export DASHSCOPE_API_KEY="your-key"`

**DeepSeek（深度求索）：**
1. 访问 [DeepSeek 开放平台](https://platform.deepseek.com/api_keys)
2. 注册或登录
3. 点击"创建 API key"
4. 复制密钥
5. 设置环境变量：`export DEEPSEEK_API_KEY="your-key"`

**Moonshot / Kimi（月之暗面）：**
1. 访问 [Moonshot 开放平台](https://platform.moonshot.cn/console/api-keys)
2. 注册或登录
3. 点击"新建 API Key"
4. 复制密钥
5. 设置环境变量：`export MOONSHOT_API_KEY="your-key"`

**OpenAI：**
1. 访问 [OpenAI API Keys](https://platform.openai.com/api-keys)
2. 注册或登录
3. 点击"Create new secret key"
4. 复制密钥
5. 设置环境变量：`export OPENAI_API_KEY="your-key"`

**Anthropic：**
1. 访问 [Anthropic Console](https://console.anthropic.com/settings/keys)
2. 注册或登录
3. 点击"Create Key"
4. 复制密钥
5. 设置环境变量：`export ANTHROPIC_API_KEY="your-key"`

**MiniMax（稀宇科技）：**
1. 访问 [MiniMax 开放平台](https://platform.minimaxi.com/)
2. 注册或登录
3. 在控制台中进入 **API Keys** 管理页面
4. 点击"创建 API Key"
5. 复制密钥
6. 设置环境变量：`export MINIMAX_API_KEY="your-key"`

**Z.AI：**
1. 访问 [Z.AI 平台](https://z.ai/)
2. 注册或登录
3. 进入 API 密钥管理页面
4. 创建新的 API 密钥
5. 复制密钥
6. 设置环境变量：`export ZAI_API_KEY="your-key"`
7. 注意：Z.AI 使用 Anthropic Messages API 协议（`api_type: "anthropic"`）

**Nvidia NIM：**
1. 访问 [Nvidia NIM](https://build.nvidia.com/)
2. 使用 Nvidia 账号注册或登录
3. 进入任意模型页面，点击"Get API Key"
4. 复制生成的密钥
5. 设置环境变量：`export NVIDIA_API_KEY="your-key"`
6. 注意：Nvidia NIM 托管多种模型 — 必须显式指定模型名称（如 `meta/llama-3.3-70b-instruct`）

**OpenRouter：**
1. 访问 [OpenRouter](https://openrouter.ai/keys)
2. 注册或登录
3. 点击"Create Key"
4. 复制密钥
5. 设置环境变量：`export OPENROUTER_API_KEY="your-key"`
6. 注意：OpenRouter 是多模型聚合器 — 使用类似 `anthropic/claude-sonnet-4-20250514`、`openai/gpt-4o` 等模型名称

### 3.2 配置方法

#### 方法 1：配置文件

在配置中设置 `provider` 和 `model`：

```json
{
  "provider": "moonshot",
  "model": "kimi-2.5",
  "api_key_env": "KIMI_API_KEY"
}
```

`api_key_env` 字段可覆盖提供商的默认环境变量名。例如 Moonshot 默认使用 `MOONSHOT_API_KEY`，但你可以改用 `KIMI_API_KEY`。

#### 方法 2：CLI 参数

```bash
octos chat --provider deepseek --model deepseek-chat
octos chat --model gpt-4o  # 从模型名称自动检测提供商
```

#### 方法 3：自动检测

省略 `provider` 时，Octos 会从模型名称自动检测提供商：

| 模型名模式 | 检测到的提供商 |
|-----------|--------------|
| `claude-*` | anthropic |
| `gpt-*`、`o1-*`、`o3-*`、`o4-*` | openai |
| `gemini-*` | gemini |
| `deepseek-*` | deepseek |
| `kimi-*`、`moonshot-*` | moonshot |
| `qwen-*` | dashscope |
| `glm-*` | zhipu |
| `llama-*` | groq |

### 3.3 自定义端点

使用 `base_url` 指向自托管或代理端点：

```json
{
  "provider": "openai",
  "model": "gpt-4o",
  "base_url": "https://your-azure-endpoint.openai.azure.com/v1"
}
```

```json
{
  "provider": "ollama",
  "model": "llama3.2",
  "base_url": "http://localhost:11434/v1"
}
```

### 3.4 API 类型覆盖

`api_type` 字段强制使用特定的 API 传输格式：

```json
{
  "provider": "zai",
  "model": "glm-5",
  "api_type": "anthropic"
}
```

- `"openai"` — OpenAI Chat Completions 格式（大多数提供商的默认值）
- `"anthropic"` — Anthropic Messages 格式（用于 Z.AI 等 Anthropic 兼容代理）

### 3.5 认证存储（OAuth 和粘贴令牌）

除了环境变量，还可以通过 auth CLI 存储 API 密钥：

```bash
# OAuth PKCE（仅 OpenAI）
octos auth login --provider openai

# 设备码流程（仅 OpenAI）
octos auth login --provider openai --device-code

# 粘贴令牌（所有其他提供商）
octos auth login --provider anthropic
# → 提示："Paste your API key:"

# 查看已存储的凭据
octos auth status

# 删除凭据
octos auth logout --provider openai
```

凭据存储在 `~/.octos/auth.json`（文件权限 0600）。解析 API 密钥时，认证存储**优先于**环境变量。

---

## 4. 故障转移与自适应路由

### 4.1 静态故障转移链

配置按优先级排序的故障转移链。如果主要提供商失败（401、403、限流、5xx），自动尝试链中的下一个提供商：

```json
{
  "provider": "moonshot",
  "model": "kimi-2.5",
  "fallback_models": [
    {
      "provider": "deepseek",
      "model": "deepseek-chat",
      "api_key_env": "DEEPSEEK_API_KEY"
    },
    {
      "provider": "gemini",
      "model": "gemini-2.0-flash",
      "api_key_env": "GEMINI_API_KEY"
    }
  ]
}
```

**故障转移规则：**
- 401/403（认证错误）→ 立即故障转移（不重试同一提供商）
- 429（限流）/ 5xx（服务器错误）→ 指数退避重试，然后故障转移
- 熔断器：连续 3 次失败 → 提供商标记为降级

### 4.2 自适应路由

配置多个备用模型后，启用自适应路由可根据实时指标动态选择最佳提供商：

```json
{
  "adaptive_routing": {
    "enabled": true,
    "latency_threshold_ms": 30000,
    "error_rate_threshold": 0.3,
    "probe_probability": 0.1,
    "probe_interval_secs": 60,
    "failure_threshold": 3
  }
}
```

- **`latency_threshold_ms`** — 平均延迟超过此值的提供商被降权（默认：30 秒）
- **`error_rate_threshold`** — 错误率超过此值的提供商被降低优先级（默认：30%）
- **`probe_probability`** — 发送到非主要提供商的探测请求比例（默认：10%）
- **`probe_interval_secs`** — 同一提供商两次探测之间的最小间隔（默认：60 秒）
- **`failure_threshold`** — 连续失败次数后触发熔断器（默认：3）

启用自适应路由后，它将替代静态优先级链，基于延迟和错误率指标进行动态选择。

---

## 5. 搜索 API 配置

`web_search` 工具使用多个搜索提供商，支持自动故障转移。

### 5.1 支持的搜索提供商

| 提供商 | 环境变量 | 费用 | 说明 |
|--------|----------|------|------|
| DuckDuckGo | *（无需）* | 免费 | 始终可用，HTML 抓取作为兜底 |
| Brave Search | `BRAVE_API_KEY` | 免费额度：每月 2K 次 | REST API |
| You.com | `YDC_API_KEY` | 付费 | 丰富的 JSON 结果和摘要 |
| Perplexity Sonar | `PERPLEXITY_API_KEY` | 付费 | AI 合成答案并附带引用 |

### 5.2 提供商选择

提供商按顺序尝试：**DuckDuckGo → Brave → You.com → Perplexity**。第一个返回非空结果的提供商获胜。如果全部失败，返回 DuckDuckGo 结果作为兜底。

设置对应的 API 密钥即可使用特定提供商：

```bash
export BRAVE_API_KEY="your-brave-key"
# 或
export PERPLEXITY_API_KEY="pplx-your-key"
```

### 5.3 配置默认结果数量

```
/config set web_search.count 10
```

此设置跨会话持久化，适用于所有搜索，除非调用方显式提供 `count`。

### 5.4 聊天使用示例

```
用户：搜索一下最新的 Rust 1.85 发布说明

机器人：[使用 web_search 工具搜索 "Rust 1.85 release notes"]
       以下是 Rust 1.85 的新特性摘要...
```

---

## 6. 工具配置

可以在运行时使用 `/config` 斜杠命令配置工具。设置持久化到 `{data_dir}/tool_config.json`。

### 6.1 可配置的工具

| 工具 | 设置项 | 类型 | 默认值 | 说明 |
|------|--------|------|--------|------|
| `news_digest` | `language` | `"zh"` / `"en"` | `"zh"` | 新闻摘要输出语言 |
| `news_digest` | `hn_top_stories` | 5-100 | 30 | 获取的 Hacker News 故事数量 |
| `news_digest` | `max_rss_items` | 5-100 | 30 | 每个 RSS 源的条目数量 |
| `news_digest` | `max_deep_fetch_total` | 1-50 | 20 | 深度获取的文章总数 |
| `news_digest` | `max_source_chars` | 1000-50000 | 12000 | 每个来源的 HTML 字符限制 |
| `news_digest` | `max_article_chars` | 1000-50000 | 8000 | 每篇文章的内容字符限制 |
| `deep_crawl` | `page_settle_ms` | 500-10000 | 3000 | JS 渲染等待时间（毫秒） |
| `deep_crawl` | `max_output_chars` | 10000-200000 | 50000 | 输出截断限制 |
| `web_search` | `count` | 1-10 | 5 | 默认搜索结果数量 |
| `web_fetch` | `extract_mode` | `"markdown"` / `"text"` | `"markdown"` | 内容提取格式 |
| `web_fetch` | `max_chars` | 1000-200000 | 50000 | 内容大小限制 |
| `browser` | `action_timeout_secs` | 30-600 | 300 | 单次操作超时 |
| `browser` | `idle_timeout_secs` | 60-600 | 300 | 空闲会话超时 |

### 6.2 聊天中的配置命令

```
/config                              # 显示所有工具设置
/config web_search                   # 显示 web_search 设置
/config set web_search.count 10      # 设置默认结果数为 10
/config set news_digest.language en  # 切换新闻摘要为英文
/config reset web_search.count       # 重置为默认值（5）
```

### 6.3 优先级顺序

设置值按以下顺序解析（最高优先级在前）：
1. 显式的每次调用参数（工具调用时传入的参数）
2. `/config` 覆盖（存储在 `tool_config.json` 中）
3. 硬编码默认值

---

## 7. 工具策略

工具策略控制智能体可以使用哪些工具。可以全局设置、按提供商设置或按上下文设置。

### 7.1 全局策略

```json
{
  "tool_policy": {
    "allow": ["group:fs", "group:search", "web_search"],
    "deny": ["shell", "spawn"]
  }
}
```

- **`allow`** — 如果非空，只允许列出的工具。如果为空，允许所有工具。
- **`deny`** — 这些工具始终被禁止。**deny 优先于 allow。**

### 7.2 命名分组

| 分组 | 展开为 |
|------|--------|
| `group:fs` | `read_file`、`write_file`、`edit_file`、`diff_edit` |
| `group:runtime` | `shell` |
| `group:web` | `web_search`、`web_fetch`、`browser` |
| `group:search` | `glob`、`grep`、`list_dir` |
| `group:sessions` | `spawn` |

### 7.3 通配符匹配

后缀 `*` 表示前缀匹配：

```json
{
  "tool_policy": {
    "deny": ["web_*"]
  }
}
```

这将禁止 `web_search`、`web_fetch` 等。

### 7.4 按提供商策略

为不同的 LLM 模型设置不同的工具集：

```json
{
  "tool_policy_by_provider": {
    "openai/gpt-4o-mini": {
      "deny": ["shell", "write_file"]
    },
    "gemini": {
      "deny": ["diff_edit"]
    }
  }
}
```

模型级别的键（如 `openai/gpt-4o-mini`）优先于提供商级别的键（如 `gemini`）。

### 7.5 标签过滤

使用 `context_filter` 限制工具到特定标签：

```json
{
  "context_filter": ["gateway"]
}
```

只有具有至少一个匹配标签的工具才可用。没有标签的工具始终通过（它们是"通用"的）。

---

## 8. 配置文件管理

配置文件是通过管理仪表盘或 API 管理的机器人实例。每个配置文件拥有独立的配置、数据目录和 gateway 进程。

### 8.1 创建配置文件

#### 通过仪表盘

1. 在仪表盘上点击"新建配置文件"
2. 填写：ID（标识符）、显示名称、提供商、模型、API 密钥环境变量
3. 添加通道（Telegram 令牌、WhatsApp 桥接 URL 等）
4. 设置系统提示词
5. 点击"创建"

#### 通过管理 API

```bash
curl -X POST http://localhost:3000/api/admin/profiles \
  -H "Content-Type: application/json" \
  -d '{
    "id": "my-bot",
    "name": "我的机器人",
    "enabled": false,
    "config": {
      "provider": "moonshot",
      "model": "kimi-2.5",
      "api_key_env": "KIMI_API_KEY",
      "gateway": {
        "channels": [
          {"type": "telegram", "allowed_senders": ["123456789"]}
        ],
        "system_prompt": "你是一个有用的助手。"
      }
    }
  }'
```

### 8.2 配置文件生命周期（启动/停止/重启）

#### 通过仪表盘

使用每个配置文件卡片上的"启动"/"停止"/"重启"按钮。

#### 通过管理 API

```bash
# 启动配置文件的 gateway
curl -X POST http://localhost:3000/api/admin/profiles/my-bot/start

# 停止配置文件的 gateway
curl -X POST http://localhost:3000/api/admin/profiles/my-bot/stop

# 重启（停止 + 启动）
curl -X POST http://localhost:3000/api/admin/profiles/my-bot/restart

# 检查状态
curl http://localhost:3000/api/admin/profiles/my-bot/status
```

**启动验证：** 启动端点会在启动 gateway 之前验证 LLM 提供商是否已配置。如果提供商或 API 密钥缺失，将返回错误。

### 8.3 更新配置文件

更新使用 **JSON 合并** — 只有你包含的字段会被修改。所有其他字段保持不变。

```bash
curl -X PUT http://localhost:3000/api/admin/profiles/my-bot \
  -H "Content-Type: application/json" \
  -d '{
    "name": "更新后的机器人名称",
    "config": {
      "model": "kimi-k2.5",
      "fallback_models": [
        {"provider": "deepseek", "model": "deepseek-chat"}
      ]
    }
  }'
```

### 8.4 删除配置文件

```bash
curl -X DELETE http://localhost:3000/api/admin/profiles/my-bot
```

这将停止 gateway 进程（如果正在运行）并级联删除所有子账户。

### 8.5 查看日志

```bash
# SSE 日志流（实时）
curl http://localhost:3000/api/admin/profiles/my-bot/logs

# 提供商指标
curl http://localhost:3000/api/admin/profiles/my-bot/metrics
```

### 8.6 API 总览端点

```bash
# 获取所有配置文件的摘要
curl http://localhost:3000/api/admin/overview
```

返回总数、运行中/已停止数量以及每个配置文件的状态。

### 8.7 测试提供商

部署前测试提供商配置：

```bash
curl -X POST http://localhost:3000/api/admin/test-provider \
  -H "Content-Type: application/json" \
  -d '{
    "provider": "moonshot",
    "model": "kimi-2.5",
    "api_key_env": "KIMI_API_KEY"
  }'
```

返回成功/失败以及提供商的响应。

---

## 9. 子账户管理

子账户是继承父配置文件 LLM 提供商设置的子机器人实例，但拥有自己的数据目录（记忆、会话、技能）和消息通道。

### 9.1 子账户工作原理

- **继承自父级：** LLM 提供商、模型、API 密钥、故障转移链
- **独有的：** 数据目录、会话、记忆、技能、系统提示词、通道
- **ID 格式：** `{父级ID}--{标识符}`（例如 `dspfac--work-bot`）
- **管理方式：** 通过 `manage_account` 工具（聊天中使用）或管理 API

### 9.2 聊天中的子账户管理

内置的 `account-manager` 技能提供了 `manage_account` 工具。用户可以通过自然对话管理子账户：

#### 列出子账户

```
用户：显示我所有的子账户

机器人：[使用 manage_account 工具，action="list"]
       以下是你的子账户：
       1. work-bot (dspfac--work-bot) - 运行中
       2. news-bot (dspfac--news-bot) - 已停止
```

#### 创建子账户

```
用户：创建一个叫"编程助手"的子账户，系统提示词是"你是一个专注于 Rust 和 Python 的编程专家"，
     连接 Telegram，令牌是 123:ABC...

机器人：[使用 manage_account 工具，action="create"]
       已创建子账户"编程助手"(dspfac--coding-assistant)。
       Telegram 通道已配置。正在启动...
```

#### 启动/停止/重启

```
用户：停止 work-bot 子账户
机器人：[使用 manage_account，action="stop"，sub_account_id="dspfac--work-bot"]
       已停止 work-bot。

用户：重启 news-bot
机器人：[使用 manage_account，action="restart"，sub_account_id="dspfac--news-bot"]
       已重启 news-bot。
```

#### 更新配置

```
用户：更新 work-bot，只允许 Telegram 用户 5460262597 发消息

机器人：[使用 manage_account，action="update"，sub_account_id="dspfac--work-bot"，
        telegram_senders="5460262597"]
       已更新 work-bot：Telegram 发送者白名单设置为 [5460262597]。
```

#### 删除子账户

```
用户：删除 old-bot 子账户

机器人：[使用 manage_account，action="delete"，sub_account_id="dspfac--old-bot"]
       已删除子账户 old-bot 及其所有数据。
```

#### 查看子账户详情

```
用户：显示 work-bot 的详细信息

机器人：[使用 manage_account，action="info"，sub_account_id="dspfac--work-bot"]
       work-bot (dspfac--work-bot)：
       - 状态：运行中
       - 通道：Telegram（允许：5460262597）
       - 系统提示词："你是一个工作助手。"
```

---

## 10. 聊天中切换模型

`switch_model` 工具允许用户在聊天中列出可用的 LLM 提供商并在运行时切换模型。此工具仅在 gateway 模式下可用。

### 10.1 列出可用提供商

```
用户：有哪些可用的模型？

机器人：[使用 switch_model 工具，action="list"]
       当前模型：moonshot/kimi-2.5

       可用提供商：
         - anthropic（默认：claude-sonnet-4-20250514）[就绪]
         - openai（默认：gpt-4o）[就绪]
         - deepseek（默认：deepseek-chat）[就绪]
         - gemini（默认：gemini-2.0-flash）[就绪]
         - moonshot（默认：kimi-k2.5）[就绪] [别名：kimi]
         - ollama（默认：llama3.2）[无需密钥]
         ...

       已配置的备用模型：
         - deepseek/deepseek-chat
```

### 10.2 切换模型

```
用户：切换到 deepseek-chat

机器人：[使用 switch_model 工具，action="switch"，model="deepseek-chat"]
       已切换到 deepseek/deepseek-chat。
       之前的模型（moonshot/kimi-2.5）已保留为备用。
```

```
用户：改用 GPT-4o

机器人：[使用 switch_model 工具，action="switch"，model="gpt-4o"]
       已切换到 openai/gpt-4o。
       之前的模型（moonshot/kimi-2.5）已保留为备用。
```

### 10.3 故障转移机制

切换模型时，之前的模型自动成为备用：
- 如果新模型失败（限流、服务器错误），请求自动回退到原始模型
- 备用机制使用现有的 `ProviderChain` 熔断器（连续 3 次失败触发故障转移）
- 链始终是扁平的：`[新模型, 原始模型]` — 重复切换不会嵌套

### 10.4 自定义提供商设置

```
用户：切换到我自定义端点上的模型

机器人：[使用 switch_model，action="switch"，model="my-model"，
        provider="openai"，base_url="https://my-endpoint.com/v1"，
        api_key_env="MY_API_KEY"]
       已切换到 openai/my-model。
```

### 10.5 持久化

模型切换会持久化到配置文件的 JSON 文件中。gateway 重启时，机器人将使用最后选择的模型启动。

---

## 11. 聊天功能与命令

### 11.1 斜杠命令

| 命令 | 说明 |
|------|------|
| `/new` | 分叉对话 — 创建新会话，复制最后 10 条消息 |
| `/config` | 查看和修改工具配置（见[第 6 节](#6-工具配置)） |
| `/exit`、`/quit`、`:q` | 退出聊天（仅 CLI 模式） |

### 11.2 会话管理

每个 通道:聊天ID 对维护独立的会话（对话历史）。

- **会话持久化：** `.octos/sessions/` 中的 JSONL 文件
- **最大历史记录：** 通过 `gateway.max_history` 配置（默认：50 条消息）
- **会话分叉：** `/new` 创建带有 parent_key 追踪的分支对话
- **上下文压缩：** 当对话超过 LLM 的上下文窗口时，较旧的消息会自动压缩（工具参数被剥离，早期消息被摘要）

### 11.3 记忆系统

智能体跨会话维护长期记忆：

- **`MEMORY.md`** — 持久化笔记，始终加载到上下文中
- **每日笔记** — `.octos/memory/YYYY-MM-DD.md`，自动创建
- **近期记忆** — 最近 7 天的每日笔记包含在上下文中
- **回忆录** — 任务完成摘要存储在 `episodes.redb` 中

```
用户：记住我偏好用 Python 写脚本，用 Rust 做系统开发。

机器人：我已经记录到记忆中了。
       [写入 MEMORY.md]
```

### 11.4 定时任务（Cron）

智能体可以使用 `cron` 工具安排定期任务：

```
用户：每天北京时间早上 8 点安排一个新闻摘要

机器人：[使用 cron 工具]
       已创建定时任务"daily-news"，每天亚洲/上海时间 8:00 运行。
       表达式：0 0 8 * * * *
```

```
用户：显示我的定时任务

机器人：[使用 cron 工具，action="list"]
       活跃的定时任务：
       1. daily-news — "生成新闻摘要" — 0 0 8 * * * *（Asia/Shanghai）— 已启用
```

也可以通过 CLI 管理定时任务：

```bash
octos cron list                              # 列出活跃任务
octos cron list --all                        # 包含已禁用的
octos cron add --name "report" --message "生成日报" --cron "0 0 9 * * * *"
octos cron add --name "check" --message "检查状态" --every 3600
octos cron remove <job-id>
octos cron enable <job-id>
octos cron enable <job-id> --disable
```

### 11.5 多轮工具使用

智能体可以在单次响应中依序使用多个工具：

```
用户：找到项目中所有 Python 文件，然后搜索 TODO 注释

机器人：[使用 glob 工具查找 *.py 文件]
       [使用 grep 工具搜索 TODO]
       找到 12 个 Python 文件中的 5 条 TODO 注释：
       - src/main.py:42: # TODO: 添加错误处理
       ...
```

### 11.6 文件操作

```
用户：读取 /etc/nginx/nginx.conf 配置文件

机器人：[使用 read_file 工具]
       以下是 nginx.conf 的内容：
       ...
```

```
用户：创建一个获取天气数据的 Python 脚本

机器人：[使用 write_file 工具]
       已创建 weather.py，内容如下...
```

### 11.7 Shell 命令

```
用户：运行测试套件

机器人：[使用 shell 工具：cargo test --workspace]
       所有 464 个测试通过。
```

### 11.8 网页浏览

```
用户：打开 https://example.com 并截图

机器人：[使用 browser 工具导航并截图]
       以下是 example.com 的截图...
```

### 11.9 子智能体

```
用户：深入研究这个主题，使用子智能体

机器人：[使用 spawn 工具创建子智能体执行研究任务]
       子智能体发现了以下内容...
```

子智能体可以通过 `sub_providers` 使用不同的 LLM 模型：

```json
{
  "sub_providers": [
    {
      "key": "cheap",
      "provider": "deepseek",
      "model": "deepseek-chat",
      "description": "适用于简单任务的快速模型"
    }
  ]
}
```

### 11.10 消息队列模式

当用户在智能体处理中发送消息时：

- **`followup`**（默认）：排队的消息按 FIFO 逐条处理
- **`collect`**：同一会话的消息被拼接后一次性处理

```json
{
  "gateway": {
    "queue_mode": "collect"
  }
}
```

### 11.11 心跳

心跳服务每 30 分钟读取 `.octos/HEARTBEAT.md` 并将其内容发送给智能体。用于后台任务指令：

```markdown
<!-- .octos/HEARTBEAT.md -->
检查 GitHub 仓库中的新 issue，汇总所有紧急问题。
```

---

## 12. 内置应用技能

内置应用技能作为编译好的二进制文件随 `octos` 一起发布。它们在 gateway 启动时自动安装到 `.octos/skills/` — 无需手动安装。

### 12.1 新闻获取

**工具名称：** `news_fetch`
**始终激活：** 是（自动包含在每次对话中）

从 Google News RSS、Hacker News API、Yahoo News、Substack 和 Medium 获取原始新闻标题和全文。工具返回原始数据 — 智能体将其合成为格式化的摘要。

#### 参数

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `categories` | 字符串数组 | 全部 | 要获取的新闻分类 |
| `language` | `"zh"` / `"en"` | `"zh"` | 输出摘要的语言 |

**可用分类：** `politics`（政治）、`world` / `international`（国际）、`business` / `commerce`（商业）、`technology` / `tech`（科技）、`science`（科学）、`entertainment` / `social`（娱乐）、`health`（健康）、`sports`（体育）

#### 聊天使用示例

```
用户：给我今天的科技和国际新闻

机器人：[使用 news_fetch，categories=["tech", "world"]，language="zh"]
       📰 科技新闻：
       1. AI 初创公司完成 5 亿美元 C 轮融资...
       2. MIT 量子计算新突破...

       🌍 国际新闻：
       1. 欧盟通过新数字法规...
       ...
```

```
用户：请生成今日新闻速递

机器人：[使用 news_fetch，language="zh"]
       📰 今日新闻速递

       🔬 科技：
       1. OpenAI 发布新模型...

       💼 商业：
       ...
```

#### 定时调度

```
用户：每天上海时间早上 8 点安排新闻摘要

机器人：[创建定时任务]
       完成！我会每天早上 8:00（Asia/Shanghai）发送新闻摘要。
```

#### 配置

```
/config set news_digest.language en          # 英文输出
/config set news_digest.hn_top_stories 50    # 获取 50 条 HN 故事
/config set news_digest.max_deep_fetch_total 30  # 深度获取 30 篇文章
```

---

### 12.2 深度搜索

**工具名称：** `deep_search`
**超时：** 600 秒（10 分钟）

多轮网络研究工具。执行多角度迭代搜索、并行页面爬取、参考文献追踪，并生成结构化报告。

#### 参数

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `query` | 字符串 | *（必填）* | 研究主题或问题 |
| `depth` | 1-3 | 2 | 研究深度级别 |
| `max_results` | 1-10 | 8 | 每轮搜索的结果数 |
| `search_engine` | 字符串 | 自动 | `perplexity`、`duckduckgo`、`brave`、`you` |

**深度级别：**
- **1（快速）：** 单轮搜索 + 爬取，约 1 分钟，最多 10 页
- **2（标准）：** 3 轮搜索 + 参考追踪，约 3 分钟，最多 30 页
- **3（深入）：** 5 轮搜索 + 积极链接追踪，约 5 分钟，最多 50 页

#### 输出文件

结果保存到 `./research/<查询标识>/`：
- `_report.md` — 结构化研究报告
- `_search_results.md` — 合并的原始搜索结果
- `01_<域名>.md`、`02_<域名>.md` 等 — 每页完整内容

#### 必需的环境变量

至少一个：`PERPLEXITY_API_KEY`（推荐）、`BRAVE_API_KEY` 或 `YDC_API_KEY`。如果都未设置，回退到 DuckDuckGo。

#### 聊天使用示例

```
用户：研究一下全球 AI 监管的现状

机器人：[使用 deep_search，query="AI regulations worldwide 2026"，depth=2]
       🔍 深度研究报告：全球 AI 监管

       ## 摘要
       各地区的 AI 监管差异显著...

       ## 主要发现
       1. **欧盟 AI 法案** — 2024 年生效...
       2. **美国行政令** — ...
       3. **中国的方案** — ...

       ## 来源
       - [EU AI Act 官方文本](...)
       - [国会研究服务](...)
       ...

       完整报告已保存至：./research/ai-regulations-worldwide-2026/
```

```
用户：快速搜索 Rust 异步运行时的对比

机器人：[使用 deep_search，query="Rust async runtime comparison tokio vs async-std"，
        depth=1]
       快速研究发现：
       - **Tokio** 是使用最广泛的（生态系统 90%+）...
```

---

### 12.3 深度爬取

**工具名称：** `deep_crawl`
**需要：** 系统中安装 Google Chrome/Chromium

使用无头 Chrome 通过 CDP（Chrome DevTools Protocol）递归爬取网站。渲染 JavaScript、通过 BFS 跟踪同源链接、从每个页面提取干净文本。

#### 参数

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `url` | 字符串 | *（必填）* | 起始 URL |
| `max_depth` | 1-10 | 3 | 最大链接跟踪深度 |
| `max_pages` | 1-200 | 50 | 最大爬取页面数 |
| `path_prefix` | 字符串 | 无 | 仅跟踪具有此路径前缀的链接 |

#### 输出

爬取的页面保存到 `crawl-<主机名>/` 目录：
- `000_index.md` — 着陆页
- `001_docs_install.md` — 第一个发现的页面
- `002_...` — 等等

#### 聊天使用示例

```
用户：爬取 docs.rs/tokio 的文档，限制在 guide 部分

机器人：[使用 deep_crawl，url="https://docs.rs/tokio/latest/tokio/"，
        max_depth=3，max_pages=30，path_prefix="/tokio/"]
       已爬取 docs.rs/tokio 的 28 个页面：

       站点地图：
       - /tokio/（索引）
       - /tokio/runtime/（运行时模块）
       - /tokio/sync/（同步原语）
       ...

       完整内容已保存至：crawl-docs.rs/
```

#### 配置

```
/config set deep_crawl.page_settle_ms 5000      # 等待 5 秒 JS 渲染
/config set deep_crawl.max_output_chars 100000   # 更大的输出限制
```

---

### 12.4 发送邮件

**工具名称：** `send_email`

通过 SMTP 或飞书邮件 API 发送邮件。

#### 参数

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `to` | 字符串 | *（必填）* | 收件人邮箱地址 |
| `subject` | 字符串 | *（必填）* | 邮件主题 |
| `body` | 字符串 | *（必填）* | 邮件正文（纯文本或 HTML） |
| `provider` | `"smtp"` / `"feishu"` | 自动 | 根据可用环境变量自动检测 |
| `html` | 布尔值 | false | 将正文视为 HTML |
| `attachments` | 数组 | 无 | 文件附件（仅 SMTP） |

#### SMTP 环境变量

```bash
export SMTP_HOST="smtp.gmail.com"
export SMTP_PORT="465"
export SMTP_USERNAME="your-email@gmail.com"
export SMTP_PASSWORD="your-app-password"
export SMTP_FROM="your-email@gmail.com"
```

#### 飞书邮件环境变量

```bash
export LARK_APP_ID="cli_..."
export LARK_APP_SECRET="..."
export LARK_FROM_ADDRESS="your-feishu-email@company.com"
# 可选：LARK_REGION="global" 使用 larksuite.com（默认：feishu.cn）
```

#### 聊天使用示例

```
用户：发一封邮件给 john@example.com，主题是"会议纪要"，包含今天的会议总结

机器人：[使用 send_email 工具]
       邮件已发送至 john@example.com，主题为"会议纪要"。
```

```
用户：发送 HTML 格式的新闻简报给 newsletter@example.com

机器人：[使用 send_email，html=true]
       HTML 邮件已发送至 newsletter@example.com。
```

```
用户：把 report.pdf 邮件发给团队负责人

机器人：[使用 send_email，attachments=[{path: "/path/to/report.pdf"}]]
       已将附带 report.pdf 附件的邮件发送至 team-lead@example.com。
```

---

### 12.5 账户管理器

**工具名称：** `manage_account`

管理当前配置文件下的子账户。详细使用方法和示例请参见[第 9 节](#9-子账户管理)。

#### 操作

| 操作 | 说明 |
|------|------|
| `list` | 列出所有子账户 |
| `create` | 创建新子账户 |
| `update` | 更新子账户设置 |
| `delete` | 删除子账户 |
| `info` | 获取子账户详情 |
| `start` | 启动子账户的 gateway |
| `stop` | 停止子账户的 gateway |
| `restart` | 重启子账户的 gateway |

#### 聊天使用示例

```
用户：为我的工作团队创建一个子账户，配置 Telegram 机器人

机器人：[使用 manage_account，action="create"，name="work team"，
        system_prompt="你是工程团队的工作助手。"，
        telegram_token="123:ABC..."，enable=true]
       已创建子账户"work team"(mybot--work-team)并启动。
       Telegram 机器人已激活。
```

---

### 12.6 时钟

**工具名称：** `get_time`
**超时：** 5 秒
**需要网络：** 否
**上下文触发：** 当对话提到"时间"、"时钟"、"几点"、"现在时间"等关键词时激活

返回任意时区的当前日期、时间、星期几和 UTC 偏移量。

#### 参数

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `timezone` | 字符串 | 服务器本地时间 | IANA 时区名称 |

**常用时区：** `UTC`、`US/Eastern`、`US/Central`、`US/Pacific`、`Europe/London`、`Europe/Paris`、`Europe/Stockholm`、`Europe/Berlin`、`Asia/Shanghai`、`Asia/Tokyo`、`Asia/Seoul`、`Asia/Singapore`、`Australia/Sydney`

#### 聊天使用示例

```
用户：东京现在几点？

机器人：[使用 get_time，timezone="Asia/Tokyo"]
       东京现在是 2026 年 3 月 6 日星期四下午 2:30（JST，UTC+9）。
```

```
用户：现在纽约几点？

机器人：[使用 get_time，timezone="US/Eastern"]
       纽约现在是凌晨 12:30，2026 年 3 月 6 日，星期五（EST，UTC-5）。
```

---

### 12.7 天气

**工具名称：** `get_weather`、`get_forecast`
**超时：** 15 秒
**API：** Open-Meteo（免费，无需 API 密钥）
**上下文触发：** 当对话提到"天气"、"预报"、"气温"等关键词时激活

#### get_weather 参数

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `city` | 字符串 | *（必填）* | 英文城市名，可选择附带国家 |

#### get_forecast 参数

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `city` | 字符串 | *（必填）* | 英文城市名 |
| `days` | 1-16 | 7 | 预报天数 |

**注意：** 始终使用英文城市名。非英文名称应翻译（如"北京"→"Beijing"）。

#### 聊天使用示例

```
用户：巴黎现在天气怎么样？

机器人：[使用 get_weather，city="Paris"]
       巴黎当前天气：
       🌤 多云转晴，12°C
       💧 湿度：65%
       💨 风速：15 km/h 西北风
```

```
用户：上海未来一周天气怎么样？

机器人：[使用 get_forecast，city="Shanghai"，days=7]
       上海未来 7 天天气预报：

       周四 3/6：☁ 8°C / 14°C — 多云
       周五 3/7：🌧 6°C / 11°C — 小雨
       周六 3/8：☀ 7°C / 16°C — 晴
       ...
```

```
用户：这周末纽约会下雨吗？

机器人：[使用 get_forecast，city="New York, US"，days=5]
       纽约天气预报：
       - 周六：30% 降雨概率，8°C/15°C
       - 周日：晴朗，10°C/18°C
       看起来周六可能有些小雨，但周日应该是晴天！
```

---

## 13. 平台技能 (ASR/TTS)

平台技能是服务器级别的技能，需要在 Apple Silicon 上运行 OminiX 后端。它们提供设备端语音转录和合成 — 无需云端 API。

### 13.1 前提条件

- Apple Silicon Mac（M1/M2/M3/M4）
- OminiX API 服务器运行中（通过 `octos serve` 管理）
- 已下载模型：`Qwen3-ASR-1.7B-8bit`、`Qwen3-TTS-12Hz-1.7B-CustomVoice-8bit`

### 13.2 通过仪表盘管理 OminiX

仪表盘提供以下控制：
- 启动/停止 OminiX 引擎
- 查看日志
- 下载/删除模型
- 检查服务健康状态

或通过管理 API：

```bash
# 启动 OminiX
curl -X POST http://localhost:3000/api/admin/platform-skills/ominix-api/start

# 检查健康状态
curl http://localhost:3000/api/admin/platform-skills/asr/health

# 下载模型
curl -X POST http://localhost:3000/api/admin/platform-skills/ominix-api/models/download \
  -H "Content-Type: application/json" \
  -d '{"model_id": "Qwen3-ASR-1.7B-8bit"}'

# 查看日志
curl http://localhost:3000/api/admin/platform-skills/ominix-api/logs?lines=100
```

### 13.3 语音转录 (`voice_transcribe`)

将音频文件转录为文本。

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `audio_path` | 字符串 | *（必填）* | 音频文件的绝对路径（WAV、OGG、MP3、FLAC、M4A） |
| `language` | 字符串 | `"Chinese"` | `"Chinese"`、`"English"`、`"Japanese"`、`"Korean"`、`"Cantonese"` |

```
用户：转录这个音频文件 /tmp/meeting.wav

机器人：[使用 voice_transcribe，audio_path="/tmp/meeting.wav"，language="Chinese"]
       转录结果：
       "大家好，今天的会议主要讨论三个议题..."
```

### 13.4 语音合成 (`voice_synthesize`)

使用预设语音将文本转换为语音。

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `text` | 字符串 | *（必填）* | 要合成的文本 |
| `output_path` | 字符串 | `/tmp/octos_tts_<ts>.wav` | 输出文件路径 |
| `language` | 字符串 | `"chinese"` | `"chinese"`、`"english"`、`"japanese"`、`"korean"` |
| `speaker` | 字符串 | `"vivian"` | 语音预设 |

**可用语音：**
- **英语/中文：** `vivian`、`serena`、`ryan`、`aiden`、`eric`、`dylan`
- **仅中文：** `uncle_fu`
- **日语：** `ono_anna`
- **韩语：** `sohee`

```
用户：朗读这段文字："欢迎收听每日简报"

机器人：[使用 voice_synthesize，text="欢迎收听每日简报"，
        language="chinese"，speaker="vivian"]
       [发送音频文件给用户]
```

### 13.5 语音克隆 (`voice_clone_synthesize`)

从参考音频样本克隆语音进行合成。

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `text` | 字符串 | *（必填）* | 要合成的文本 |
| `reference_audio` | 字符串 | *（必填）* | 参考音频路径（3-10 秒） |
| `output_path` | 字符串 | 自动 | 输出文件路径 |
| `language` | 字符串 | `"chinese"` | 目标语言 |

```
用户：用我的声音样本克隆并说"早上好，团队"
     参考：/tmp/my-voice-sample.wav

机器人：[使用 voice_clone_synthesize，reference_audio="/tmp/my-voice-sample.wav"，
        text="早上好，团队"，language="chinese"]
       已用你的声音生成语音。[发送音频]
```

### 13.6 播客生成 (`generate_podcast`)

从脚本创建多角色播客音频。

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `script` | 数组 | *（必填）* | `{speaker, voice, text}` 对象数组 |
| `output_path` | 字符串 | 自动 | 输出文件路径 |
| `language` | 字符串 | `"chinese"` | 语言 |

```
用户：生成一期关于 AI 安全的短播客，两个主持人

机器人：[使用 generate_podcast，script=[
        {speaker: "主持人", voice: "vivian", text: "欢迎收听 AI 周刊..."},
        {speaker: "嘉宾", voice: "ryan", text: "谢谢邀请..."},
        ...
      ]，language="chinese"]
       已生成播客（时长 2:30）。[发送音频文件]
```

### 13.7 Gateway 语音配置

消息通道中语音消息的自动转录和自动 TTS：

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

- **`auto_asr`**：自动转录收到的语音/音频消息后再发送给智能体
- **`auto_tts`**：当用户发送语音时自动合成语音回复
- **`default_voice`**：自动 TTS 的语音预设
- **`asr_language`**：强制转录语言（`null` = 自动检测）

---

## 14. 自定义技能安装

自定义技能通过新的工具和指令扩展智能体的能力。可以从 GitHub 仓库安装或在本地创建。

### 14.1 从 GitHub 安装

```bash
# 安装仓库中的所有技能
octos skills install user/repo

# 安装特定的技能子目录
octos skills install user/repo/skill-name

# 从特定分支安装
octos skills install user/repo --branch develop

# 强制覆盖已有技能
octos skills install user/repo --force

# 安装到特定配置文件
octos skills install user/repo --profile my-bot
```

**安装过程：**
1. 尝试从技能注册表下载预编译二进制文件（SHA-256 验证）
2. 如果存在 `Cargo.toml`，回退到 `cargo build --release`
3. 如果存在 `package.json`，运行 `npm install`
4. 写入 `.source` 文件用于更新追踪

### 14.2 管理技能

```bash
# 列出已安装的技能
octos skills list

# 显示技能详情
octos skills info skill-name

# 更新特定技能
octos skills update skill-name

# 更新所有技能
octos skills update all

# 删除技能
octos skills remove skill-name

# 搜索在线注册表
octos skills search "网页抓取"
```

### 14.3 技能目录结构

技能位于 `.octos/skills/<名称>/`，包含：

```
.octos/skills/my-skill/
├── SKILL.md         # 必需：指令 + frontmatter
├── manifest.json    # 工具技能必需：工具定义
├── main             # 编译好的二进制文件（或脚本）
└── .source          # 自动生成：追踪安装来源
```

### 14.4 SKILL.md 格式

```markdown
---
name: my-skill
version: 1.0.0
author: 你的名字
description: 这个技能做什么的简短描述
always: false
requires_bins: curl,jq
requires_env: MY_API_KEY
---

# 我的技能指令

告诉智能体如何以及何时使用此技能的指令。

## 使用场景
- 当用户询问关于...时使用此技能

## 工具用法
`my_tool` 工具接受：
- `query`（必填）：搜索查询
- `limit`（可选）：最大结果数（默认：10）

## 示例
用户："帮我查找关于 X 的信息"
→ 使用 my_tool，query="X"
```

**Frontmatter 字段：**
- **`name`** — 技能标识符（必须与目录名匹配）
- **`version`** — 语义版本号
- **`author`** — 技能作者
- **`description`** — 简短描述
- **`always`** — 如果为 `true`，技能指令始终包含在系统提示词中。如果为 `false`，智能体可以按需读取。
- **`requires_bins`** — 逗号分隔的二进制文件名，通过 `which` 检查。任何一个缺失则技能不可用。
- **`requires_env`** — 逗号分隔的环境变量名。任何一个未设置则技能不可用。

### 14.5 manifest.json 格式

对于提供可执行工具的技能：

```json
{
  "name": "my-skill",
  "version": "1.0.0",
  "description": "我的自定义技能",
  "tools": [
    {
      "name": "my_tool",
      "description": "做一些有用的事情",
      "timeout_secs": 60,
      "input_schema": {
        "type": "object",
        "properties": {
          "query": {
            "type": "string",
            "description": "搜索查询"
          },
          "limit": {
            "type": "integer",
            "description": "最大结果数",
            "default": 10
          }
        },
        "required": ["query"]
      }
    }
  ],
  "entrypoint": "main"
}
```

工具二进制文件通过 stdin 接收 JSON 输入，通过 stdout 输出 JSON：

```json
// 输入（stdin）
{"query": "test", "limit": 5}

// 输出（stdout）
{"output": "结果在这里...", "success": true}
```

### 14.6 技能解析顺序

技能从以下目录加载（按优先级排序）：

1. `.octos/plugins/`（旧版）
2. `.octos/skills/`（用户安装的自定义技能）
3. `.octos/bundled-app-skills/`（内置：news、deep-search 等）
4. `.octos/platform-skills/`（平台：asr/tts）
5. `~/.octos/plugins/`（全局旧版）
6. `~/.octos/skills/`（全局自定义）

用户安装的技能覆盖同名的内置技能。

### 14.7 创建自定义技能

#### 示例：翻译技能（Python）

1. 创建技能目录：

```bash
mkdir -p .octos/skills/translator
```

2. 创建 `SKILL.md`：

```markdown
---
name: translator
version: 1.0.0
description: 使用 DeepL API 在语言之间翻译文本
always: false
requires_env: DEEPL_API_KEY
---

# 翻译技能

当用户要求翻译文本时，使用 `translate` 工具。

## 用法
- `text`（必填）：要翻译的文本
- `target_lang`（必填）：目标语言代码（EN、DE、FR、JA、ZH 等）
- `source_lang`（可选）：源语言代码（省略时自动检测）
```

3. 创建 `manifest.json`：

```json
{
  "name": "translator",
  "version": "1.0.0",
  "tools": [
    {
      "name": "translate",
      "description": "使用 DeepL 在语言之间翻译文本",
      "timeout_secs": 30,
      "input_schema": {
        "type": "object",
        "properties": {
          "text": {"type": "string", "description": "要翻译的文本"},
          "target_lang": {"type": "string", "description": "目标语言代码"},
          "source_lang": {"type": "string", "description": "源语言代码"}
        },
        "required": ["text", "target_lang"]
      }
    }
  ],
  "entrypoint": "main"
}
```

4. 创建 `main`（可执行脚本）：

```python
#!/usr/bin/env python3
import json, sys, os, urllib.request

input_data = json.loads(sys.stdin.read())
text = input_data["text"]
target = input_data["target_lang"]
source = input_data.get("source_lang", "")

api_key = os.environ["DEEPL_API_KEY"]
data = json.dumps({
    "text": [text],
    "target_lang": target,
    **({"source_lang": source} if source else {})
}).encode()

req = urllib.request.Request(
    "https://api-free.deepl.com/v2/translate",
    data=data,
    headers={"Authorization": f"DeepL-Auth-Key {api_key}", "Content-Type": "application/json"}
)

with urllib.request.urlopen(req) as resp:
    result = json.loads(resp.read())
    translated = result["translations"][0]["text"]
    print(json.dumps({"output": translated, "success": True}))
```

5. 设置可执行权限：

```bash
chmod +x .octos/skills/translator/main
```

6. 测试使用：

```
用户：把"Hello world"翻译成日语

机器人：[使用 translate 工具，text="Hello world"，target_lang="JA"]
       翻译结果：こんにちは世界
```

---

## 15. 配置参考

### 15.1 完整配置结构

```json
{
  "version": 1,

  // LLM 提供商
  "provider": "anthropic",
  "model": "claude-sonnet-4-20250514",
  "base_url": null,
  "api_key_env": null,
  "api_type": null,

  // 故障转移链
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
    "latency_threshold_ms": 30000,
    "error_rate_threshold": 0.3,
    "probe_probability": 0.1,
    "probe_interval_secs": 60,
    "failure_threshold": 3
  },

  // Gateway
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

  // 子提供商（用于 spawn 工具）
  "sub_providers": [
    {
      "key": "cheap",
      "provider": "deepseek",
      "model": "deepseek-chat",
      "description": "适用于简单任务的快速模型"
    }
  ],

  // 智能体设置
  "max_iterations": 50,

  // 嵌入（用于记忆中的向量搜索）
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

  // 邮件（用于邮件通道）
  "email": null,

  // 仪表盘认证（仅 serve 模式）
  "dashboard_auth": null,

  // 监控（仅 serve 模式）
  "monitor": null
}
```

### 15.2 环境变量

| 变量 | 说明 |
|------|------|
| **LLM 提供商** | |
| `ANTHROPIC_API_KEY` | Anthropic（Claude）API 密钥 |
| `OPENAI_API_KEY` | OpenAI API 密钥 |
| `GEMINI_API_KEY` | Google Gemini API 密钥 |
| `OPENROUTER_API_KEY` | OpenRouter API 密钥 |
| `DEEPSEEK_API_KEY` | DeepSeek API 密钥 |
| `GROQ_API_KEY` | Groq API 密钥 |
| `MOONSHOT_API_KEY` | Moonshot/Kimi API 密钥 |
| `DASHSCOPE_API_KEY` | 阿里云灵积（通义千问）API 密钥 |
| `MINIMAX_API_KEY` | MiniMax API 密钥 |
| `ZHIPU_API_KEY` | 智谱（GLM）API 密钥 |
| `ZAI_API_KEY` | Z.AI API 密钥 |
| `NVIDIA_API_KEY` | Nvidia NIM API 密钥 |
| **搜索** | |
| `BRAVE_API_KEY` | Brave 搜索 API 密钥 |
| `PERPLEXITY_API_KEY` | Perplexity Sonar API 密钥 |
| `YDC_API_KEY` | You.com API 密钥 |
| **通道** | |
| `TELEGRAM_BOT_TOKEN` | Telegram 机器人令牌 |
| `DISCORD_BOT_TOKEN` | Discord 机器人令牌 |
| `SLACK_BOT_TOKEN` | Slack 机器人令牌 |
| `SLACK_APP_TOKEN` | Slack 应用级令牌 |
| `FEISHU_APP_ID` | 飞书应用 ID |
| `FEISHU_APP_SECRET` | 飞书应用密钥 |
| `WECOM_CORP_ID` | 企业微信企业 ID |
| `WECOM_AGENT_SECRET` | 企业微信应用密钥 |
| `EMAIL_USERNAME` | 邮件账户用户名 |
| `EMAIL_PASSWORD` | 邮件账户密码 |
| **邮件（send-email 技能）** | |
| `SMTP_HOST` | SMTP 服务器主机名 |
| `SMTP_PORT` | SMTP 服务器端口 |
| `SMTP_USERNAME` | SMTP 用户名 |
| `SMTP_PASSWORD` | SMTP 密码 |
| `SMTP_FROM` | SMTP 发件人地址 |
| `LARK_APP_ID` | 飞书邮件应用 ID |
| `LARK_APP_SECRET` | 飞书邮件应用密钥 |
| `LARK_FROM_ADDRESS` | 飞书邮件发件人地址 |
| **语音** | |
| `OMINIX_API_URL` | OminiX ASR/TTS API 地址 |
| **系统** | |
| `RUST_LOG` | 日志级别（error/warn/info/debug/trace） |
| `OCTOS_LOG_JSON` | 启用 JSON 格式日志（设置为任意值） |

### 15.3 文件布局

```
~/.octos/                        # 全局配置目录
├── auth.json                   # 存储的 API 凭据（权限 0600）
├── profiles/                   # 配置文件（serve 模式）
│   ├── my-bot.json
│   └── work-bot.json
├── skills/                     # 全局自定义技能
└── serve.log                   # Serve 模式日志文件

.octos/                          # 项目/配置文件数据目录
├── config.json                 # 配置
├── cron.json                   # 定时任务
├── AGENTS.md                   # 智能体指令
├── SOUL.md                     # 个性定义
├── USER.md                     # 用户信息
├── TOOLS.md                    # 工具特定指南
├── IDENTITY.md                 # 自定义身份
├── HEARTBEAT.md                # 后台任务指令
├── sessions/                   # 对话历史（JSONL）
├── memory/                     # 记忆文件
│   ├── MEMORY.md               # 长期持久化记忆
│   └── 2026-03-06.md           # 每日笔记
├── skills/                     # 自定义技能
│   ├── news/                   # 内置：新闻获取
│   ├── deep-search/            # 内置：深度搜索
│   ├── deep-crawl/             # 内置：深度爬取
│   ├── send-email/             # 内置：邮件发送
│   ├── account-manager/        # 内置：子账户管理
│   ├── clock/                  # 内置：时间查询
│   ├── weather/                # 内置：天气信息
│   └── my-custom-skill/        # 用户安装的技能
├── platform-skills/            # 平台技能（ASR/TTS）
├── episodes.redb               # 回忆录数据库
├── tool_config.json            # 工具配置覆盖
└── history/
    └── chat_history            # Readline 历史（CLI）
```

---

## 16. Matrix Appservice（Palpo）

Octos 可以作为 [Matrix Application Service](https://spec.matrix.org/latest/application-service-api/)（应用服务）运行在 Matrix 主服务器后面。本节介绍如何使用 Docker Compose 将 Octos 与 [Palpo](https://github.com/palpo-im/palpo) 一起部署，使用户可以从任何 Matrix 客户端与机器人对话。

### 16.1 工作原理

```
Matrix 客户端（Element 等）
       │
       ▼
  Palpo（主服务器 :8008）
       │  通过 Appservice API 推送事件
       ▼
  Octos（应用服务监听 :8009）
       │  通过 Palpo 的 Client-Server API 回复消息
       ▼
  Palpo ──► Matrix 客户端
```

Palpo 在启动时加载一个**注册 YAML 文件**，告诉它哪些用户命名空间属于 Octos，以及将事件转发到哪里。Octos 在专用端口（默认 `8009`）监听这些事件，并通过 Palpo 的 Client-Server API 回复。

### 16.2 目录结构

```
palpo_with_octos/
├── compose.yml                        # Docker Compose 文件
├── palpo.toml                         # Palpo 主服务器配置
├── appservices/
│   └── octos-registration.yaml        # 应用服务注册文件
├── config/
│   ├── botfather.json                 # Octos 配置文件（Matrix 频道）
│   └── octos.json                     # Octos 全局配置
├── data/
│   ├── pgsql/                         # PostgreSQL 数据
│   ├── octos/                         # Octos 运行时数据
│   └── media/                         # Palpo 媒体存储
└── static/
    └── index.html                     # Palpo 主页
```

### 16.3 配置步骤

#### 1. 生成令牌

应用服务注册文件和 Octos 配置文件必须共享两个令牌。只需生成一次：

```bash
# 生成 as_token 和 hs_token（任意随机十六进制字符串）
openssl rand -hex 32   # → as_token
openssl rand -hex 32   # → hs_token
```

保存好这两个值 — 下面两个文件都需要用到。

#### 2. 创建应用服务注册文件

创建 `appservices/octos-registration.yaml`：

```yaml
# Matrix 应用服务注册 — octos
id: octos-matrix-appservice

# Palpo 推送事件到 octos 的 URL（使用 Docker 服务名，不是 localhost）
url: "http://octos:8009"

# 令牌 — 必须与 config/botfather.json 匹配
as_token: "<你的-as-token>"
hs_token: "<你的-hs-token>"

sender_localpart: octosbot
rate_limited: false

namespaces:
  users:
    - exclusive: true
      regex: "@octosbot_.*:your\\.server\\.name"
    - exclusive: true
      regex: "@octosbot:your\\.server\\.name"
  aliases: []
  rooms: []
```

关键字段说明：

| 字段 | 说明 |
|------|------|
| `url` | Palpo 发送事件的目标地址。使用 Docker 服务名（如 `http://octos:8009`），不要用 `localhost`。 |
| `as_token` | Octos 调用 Palpo API 时使用的令牌。 |
| `hs_token` | Palpo 向 Octos 推送事件时使用的令牌。 |
| `sender_localpart` | 机器人的 Matrix 本地用户名（最终变为 `@octosbot:your.server.name`）。 |
| `namespaces.users` | 应用服务管理的用户 ID 正则匹配模式。包含机器人本身和桥接用户前缀。 |

#### 3. 配置 Palpo

在 `palpo.toml` 中，指向包含注册文件的目录：

```toml
server_name = "your.server.name"
listen_addr = "0.0.0.0:8008"

allow_registration = true
allow_federation = true

# Palpo 启动时自动加载此目录下所有 .yaml 文件
appservice_registration_dir = "/var/palpo/appservices"

[db]
url = "postgres://palpo:<数据库密码>@palpo_postgres:5432/palpo"
pool_size = 10

[well_known]
server = "your.server.name"
client = "https://your.server.name"
```

#### 4. 创建 Octos 配置文件

创建 `config/botfather.json`，配置使用相同令牌的 Matrix 频道：

```json
{
  "id": "botfather",
  "name": "BotFather",
  "enabled": true,
  "config": {
    "provider": "deepseek",
    "model": "deepseek-chat",
    "api_key_env": "DEEPSEEK_API_KEY",
    "channels": [
      {
        "type": "matrix",
        "homeserver": "http://palpo:8008",
        "as_token": "<你的-as-token>",
        "hs_token": "<你的-hs-token>",
        "server_name": "your.server.name",
        "sender_localpart": "octosbot",
        "user_prefix": "octosbot_",
        "port": 8009,
        "allowed_senders": ["@alice:your.server.name"]
      }
    ],
    "gateway": {
      "max_history": 50,
      "queue_mode": "followup"
    }
  }
}
```

Matrix 频道字段说明：

| 字段 | 说明 |
|------|------|
| `type` | 必须为 `"matrix"`。 |
| `homeserver` | Palpo 的内部 URL（Docker 服务名）。 |
| `as_token` / `hs_token` | 必须与注册 YAML 文件匹配。 |
| `server_name` | Matrix 域名（必须与 `palpo.toml` 一致）。 |
| `sender_localpart` | 机器人用户名（必须与注册文件一致）。 |
| `user_prefix` | 此应用服务管理的桥接用户 ID 前缀。 |
| `port` | Octos 监听来自 Palpo 的应用服务事件的端口。 |
| `allowed_senders` | 允许与机器人对话的 Matrix 用户 ID。空数组 = 允许所有人。 |

#### 5. Docker Compose

```yaml
services:
  palpo_postgres:
    image: postgres:17
    restart: always
    volumes:
      - ./data/pgsql:/var/lib/postgresql/data
    environment:
      POSTGRES_PASSWORD: <数据库密码>
      POSTGRES_USER: palpo
      POSTGRES_DB: palpo
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U palpo"]
      interval: 5s
      timeout: 5s
      retries: 5
    networks:
      - internal

  palpo:
    image: ghcr.io/palpo-im/palpo:latest
    restart: unless-stopped
    ports:
      - 8128:8008     # Client-Server API
      - 8348:8448     # Federation API
    environment:
      PALPO_CONFIG: "/var/palpo/palpo.toml"
    volumes:
      - ./palpo.toml:/var/palpo/palpo.toml:ro
      - ./appservices:/var/palpo/appservices:ro
      - ./data/media:/var/palpo/media
      - ./static:/var/palpo/static:ro
    depends_on:
      palpo_postgres:
        condition: service_healthy
    networks:
      - internal

  octos:
    build:
      context: /path/to/octos       # Octos 源码仓库路径
      dockerfile: Dockerfile
    restart: unless-stopped
    ports:
      - 8009:8009     # 应用服务监听（接收 Palpo 推送的事件）
      - 8010:8080     # Octos 仪表盘 / 管理 API
    environment:
      DEEPSEEK_API_KEY: ${DEEPSEEK_API_KEY}
      RUST_LOG: octos=debug,info
    volumes:
      - ./data/octos:/root/.octos
      - ./config/botfather.json:/root/.octos/profiles/botfather.json:ro
      - ./config/octos.json:/config/octos.json:ro
    command: ["serve", "--host", "0.0.0.0", "--port", "8080", "--config", "/config/octos.json"]
    depends_on:
      - palpo
    networks:
      - internal

networks:
  internal:
    attachable: true
```

#### 6. 启动所有服务

```bash
docker compose up -d
```

Palpo 在启动时读取 `appservices/octos-registration.yaml`。当 Matrix 用户在机器人所在的房间发送消息时，Palpo 将事件推送到 `http://octos:8009`，Octos 通过智能体循环处理消息，并通过 Palpo 的 Client-Server API 回复。

### 16.4 令牌匹配检查清单

最常见的配置错误是令牌不匹配。以下三处必须一致：

| 值 | `octos-registration.yaml` | `botfather.json` |
|----|--------------------------|-------------------|
| `as_token` | `as_token: "abc..."` | `"as_token": "abc..."` |
| `hs_token` | `hs_token: "def..."` | `"hs_token": "def..."` |
| `sender_localpart` | `sender_localpart: octosbot` | `"sender_localpart": "octosbot"` |
| server name | `regex: "@octosbot:your\\.server\\.name"` | `"server_name": "your.server.name"` |

### 16.5 故障排除

| 症状 | 原因 | 解决方法 |
|------|------|----------|
| 机器人无响应 | 注册文件与配置文件之间令牌不匹配 | 检查[令牌匹配清单](#164-令牌匹配检查清单) |
| Palpo 日志中出现 `Connection refused` | Octos 未运行或注册文件中 `url` 错误 | 确保 Octos 已启动；使用 Docker 服务名（`http://octos:8009`），不要用 `localhost` |
| `User ID not in namespace` | `sender_localpart` 与注册文件 `namespaces.users` 正则不匹配 | 更新正则以包含机器人的完整用户 ID |
| 未授权用户的消息被忽略 | `allowed_senders` 过滤 | 将用户的 Matrix ID 添加到数组中，或设置为 `[]` 以允许所有人 |

---

*本指南涵盖截至 2026 年 3 月的 Octos 版本。最新更新请参阅仓库 [github.com/octos-org/octos](https://github.com/octos-org/octos)。*
