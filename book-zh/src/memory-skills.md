# 记忆与技能

Octos 拥有分层记忆系统和可扩展的技能框架。记忆赋予智能体跨会话的持久上下文，技能则为智能体提供新的工具和能力。

## 引导文件

这些文件在启动时加载到系统提示词中。使用 `octos init` 创建它们。

| 文件 | 用途 |
|------|---------|
| `.octos/AGENTS.md` | 智能体指令与准则 |
| `.octos/SOUL.md` | 人格与价值观 |
| `.octos/USER.md` | 用户信息与偏好 |
| `.octos/TOOLS.md` | 工具使用指南 |
| `.octos/IDENTITY.md` | 自定义身份定义 |

引导文件支持热更新——编辑后智能体会自动获取更改，无需重启。

## 记忆系统

Octos 采用三层记忆架构：

### 长期记忆

存储在 `.octos/memory/MEMORY.md` 中。这是一个持久化的备忘录，智能体（或你）可以在此保存重要事实、偏好和笔记，这些内容会跨所有会话保留。智能体可以使用其记忆工具写入此文件。

### 每日笔记

以 `.octos/memory/YYYY-MM-DD.md` 格式存储。每天自动创建新文件。智能体在此记录重要事件、决策和观察。最近 7 天的每日笔记会自动纳入智能体的上下文，形成近期活动的滚动窗口。

### 会话历史

以 JSONL 文件形式存储在 `.octos/sessions/` 中。每个对话会话维护独立的历史记录。恢复会话时，完整历史会重新加载到上下文中。

### 记忆工具

智能体内置了管理自身记忆的工具：

- **Save** -- 将关键事实或笔记写入长期记忆或当日笔记
- **Recall** -- 搜索记忆中的相关信息

你也可以直接用任何文本编辑器编辑 `.octos/memory/MEMORY.md`。

## 文件结构

```
.octos/
├── config.json          # 配置文件（版本化，自动迁移）
├── cron.json            # 定时任务存储
├── AGENTS.md            # 智能体指令
├── SOUL.md              # 人格设定
├── USER.md              # 用户信息
├── HEARTBEAT.md         # 后台任务
├── sessions/            # 聊天历史（JSONL）
├── memory/              # 记忆文件
│   ├── MEMORY.md        # 长期记忆
│   └── 2025-02-10.md    # 每日笔记（示例）
├── skills/              # 自定义技能
├── episodes.redb        # 情景记忆数据库
└── history/
    └── chat_history     # Readline 历史
```

---

## 内置系统技能

Octos 在编译时内置了 3 个系统技能：

| 技能 | 说明 |
|-------|-------------|
| `cron` | 定时任务工具使用示例（常驻） |
| `skill-store` | 技能安装与管理 |
| `skill-creator` | 自定义技能创建指南 |

工作区中 `.octos/skills/` 下的技能会覆盖同名的内置技能。

## 预装应用技能

八个应用技能以编译后的二进制文件形式随 Octos 分发。它们在网关启动时自动部署到 `.octos/skills/`——无需手动安装。

### 新闻获取

**工具：** `news_fetch` | **常驻：** 是

从 Google News RSS、Hacker News API、Yahoo News、Substack 和 Medium 抓取头条和全文内容。智能体会将原始数据整理为格式化的新闻摘要。

**参数：**

| 参数 | 类型 | 默认值 | 说明 |
|-----------|------|---------|-------------|
| `categories` | array | 全部 | 要获取的新闻分类 |
| `language` | `"zh"` / `"en"` | `"zh"` | 输出语言 |

分类：`politics`、`world`、`business`、`technology`、`science`、`entertainment`、`health`、`sports`

**配置：**

```
/config set news_digest.language en
/config set news_digest.hn_top_stories 50
/config set news_digest.max_deep_fetch_total 30
```

### 深度搜索

**工具：** `deep_search` | **超时时间：** 600 秒

多轮网络研究工具。执行迭代搜索、并行页面爬取、引用链追踪，并生成结构化报告保存到 `./research/<query-slug>/`。

| 参数 | 类型 | 默认值 | 说明 |
|-----------|------|---------|-------------|
| `query` | string | *（必填）* | 研究主题或问题 |
| `depth` | 1--3 | 2 | 研究深度级别 |
| `max_results` | 1--10 | 8 | 每轮搜索的结果数 |
| `search_engine` | string | auto | `perplexity`、`duckduckgo`、`brave`、`you` |

**深度级别：**

- **1（快速）：** 单轮搜索，约 1 分钟，最多 10 个页面
- **2（标准）：** 3 轮搜索 + 引用链追踪，约 3 分钟，最多 30 个页面
- **3（深入）：** 5 轮搜索 + 积极链接追踪，约 5 分钟，最多 50 个页面

### 深度爬取

**工具：** `deep_crawl` | **依赖：** PATH 中需有 Chrome/Chromium

使用无头 Chrome 通过 CDP 递归爬取网站。渲染 JavaScript，通过 BFS 跟踪同源链接，提取干净文本。

| 参数 | 类型 | 默认值 | 说明 |
|-----------|------|---------|-------------|
| `url` | string | *（必填）* | 起始 URL |
| `max_depth` | 1--10 | 3 | 最大链接跟踪深度 |
| `max_pages` | 1--200 | 50 | 最大爬取页面数 |
| `path_prefix` | string | 无 | 仅跟踪此路径下的链接 |

输出保存到 `crawl-<hostname>/`，以编号的 Markdown 文件形式存储。

**配置：**

```
/config set deep_crawl.page_settle_ms 5000
/config set deep_crawl.max_output_chars 100000
```

### 发送邮件

**工具：** `send_email`

通过 SMTP 或飞书/Lark Mail API 发送邮件（根据可用的环境变量自动检测）。

| 参数 | 类型 | 默认值 | 说明 |
|-----------|------|---------|-------------|
| `to` | string | *（必填）* | 收件人邮箱地址 |
| `subject` | string | *（必填）* | 邮件主题 |
| `body` | string | *（必填）* | 邮件正文（纯文本或 HTML） |
| `html` | boolean | false | 将正文视为 HTML |
| `attachments` | array | 无 | 文件附件（仅 SMTP） |

**SMTP 环境变量：**

```bash
export SMTP_HOST="smtp.gmail.com"
export SMTP_PORT="465"
export SMTP_USERNAME="your-email@gmail.com"
export SMTP_PASSWORD="your-app-password"
export SMTP_FROM="your-email@gmail.com"
```

### 天气

**工具：** `get_weather`、`get_forecast` | **API：** Open-Meteo（免费，无需密钥）

| 参数 | 类型 | 默认值 | 说明 |
|-----------|------|---------|-------------|
| `city` | string | *（必填）* | 英文城市名 |
| `days` | 1--16 | 7 | 预报天数（仅预报） |

### 时钟

**工具：** `get_time`

返回任意 IANA 时区的当前日期、时间、星期和 UTC 偏移。

| 参数 | 类型 | 默认值 | 说明 |
|-----------|------|---------|-------------|
| `timezone` | string | 服务器本地时区 | IANA 时区名称（如 `Asia/Shanghai`、`US/Eastern`） |

### 账户管理

**工具：** `manage_account`

管理当前配置下的子账户。操作：`list`、`create`、`update`、`delete`、`info`、`start`、`stop`、`restart`。

---

## 平台技能（ASR/TTS）

平台技能提供设备端语音转写和合成。需要在 Apple Silicon（M1/M2/M3/M4）上运行 OminiX 后端。

### 语音转写

**工具：** `voice_transcribe`

| 参数 | 类型 | 默认值 | 说明 |
|-----------|------|---------|-------------|
| `audio_path` | string | *（必填）* | 音频文件路径（WAV、OGG、MP3、FLAC、M4A） |
| `language` | string | `"Chinese"` | `"Chinese"`、`"English"`、`"Japanese"`、`"Korean"`、`"Cantonese"` |

### 语音合成

**工具：** `voice_synthesize`

| 参数 | 类型 | 默认值 | 说明 |
|-----------|------|---------|-------------|
| `text` | string | *（必填）* | 要合成的文本 |
| `output_path` | string | 自动 | 输出文件路径 |
| `language` | string | `"chinese"` | `"chinese"`、`"english"`、`"japanese"`、`"korean"` |
| `speaker` | string | `"vivian"` | 语音预设 |

**可用语音：** `vivian`、`serena`、`ryan`、`aiden`、`eric`、`dylan`（英/中）、`uncle_fu`（仅中文）、`ono_anna`（日语）、`sohee`（韩语）

### 语音克隆

**工具：** `voice_clone_synthesize`

使用 3--10 秒参考音频样本的克隆语音进行语音合成。

| 参数 | 类型 | 默认值 | 说明 |
|-----------|------|---------|-------------|
| `text` | string | *（必填）* | 要合成的文本 |
| `reference_audio` | string | *（必填）* | 参考音频路径 |
| `language` | string | `"chinese"` | 目标语言 |

### 播客生成

**工具：** `generate_podcast`

根据 `{speaker, voice, text}` 对象组成的脚本创建多说话人播客音频。

---

## 自定义技能安装

### 从 GitHub 安装

```bash
# 安装仓库中的所有技能
octos skills install user/repo

# 安装特定技能
octos skills install user/repo/skill-name

# 从指定分支安装
octos skills install user/repo --branch develop

# 强制覆盖已有技能
octos skills install user/repo --force

# 安装到指定配置文件
octos skills install user/repo --profile my-bot
```

安装程序会优先从技能注册表下载预编译二进制文件（SHA-256 校验），如有 `Cargo.toml` 则回退到 `cargo build --release`，如有 `package.json` 则运行 `npm install`。

### 技能管理

```bash
octos skills list                    # 列出已安装的技能
octos skills info skill-name         # 查看技能详情
octos skills update skill-name       # 更新指定技能
octos skills update all              # 更新所有技能
octos skills remove skill-name       # 删除技能
octos skills search "web scraping"   # 搜索在线注册表
```

### 技能解析顺序

技能按以下目录加载（优先级从高到低）：

1. `.octos/plugins/`（旧版兼容）
2. `.octos/skills/`（用户安装的自定义技能）
3. `.octos/bundled-app-skills/`（预装应用技能）
4. `.octos/platform-skills/`（平台技能：ASR/TTS）
5. `~/.octos/plugins/`（全局旧版兼容）
6. `~/.octos/skills/`（全局自定义技能）

用户安装的技能会覆盖同名的预装技能。

---

## 技能开发

自定义技能位于 `.octos/skills/<name>/` 目录下，包含：

```
.octos/skills/my-skill/
├── SKILL.md         # 必需：指令 + frontmatter
├── manifest.json    # 工具技能必需：工具定义
├── main             # 编译后的二进制文件（或脚本）
└── .source          # 自动生成：追踪安装来源
```

### SKILL.md 格式

```markdown
---
name: my-skill
version: 1.0.0
author: Your Name
description: A brief description of what this skill does
always: false
requires_bins: curl,jq
requires_env: MY_API_KEY
---

# My Skill Instructions

Instructions for the agent on how and when to use this skill.

## When to Use
- Use this skill when the user asks about...

## Tool Usage
The `my_tool` tool accepts:
- `query` (required): The search query
- `limit` (optional): Maximum results (default: 10)
```

**Frontmatter 字段：**

| 字段 | 说明 |
|-------|-------------|
| `name` | 技能标识符（须与目录名一致） |
| `version` | 语义化版本号 |
| `author` | 技能作者 |
| `description` | 简短描述 |
| `always` | 为 `true` 时，每次系统提示词都会包含该技能；为 `false` 时，按需加载 |
| `requires_bins` | 逗号分隔的二进制文件列表，通过 `which` 检查。任一缺失则技能不可用 |
| `requires_env` | 逗号分隔的环境变量列表。任一未设置则技能不可用 |

### manifest.json 格式

用于提供可执行工具的技能：

```json
{
  "name": "my-skill",
  "version": "1.0.0",
  "description": "My custom skill",
  "tools": [
    {
      "name": "my_tool",
      "description": "Does something useful",
      "timeout_secs": 60,
      "input_schema": {
        "type": "object",
        "properties": {
          "query": {
            "type": "string",
            "description": "The search query"
          },
          "limit": {
            "type": "integer",
            "description": "Maximum results",
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

工具二进制文件通过 stdin 接收 JSON 输入，须通过 stdout 输出 JSON：

```json
// 输入 (stdin)
{"query": "test", "limit": 5}

// 输出 (stdout)
{"output": "Results here...", "success": true}
```
