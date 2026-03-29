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

Octos 采用三层记忆架构，结合自动记录与智能体驱动的知识管理：

```
┌──────────────────────────────────────────────────────────────────┐
│                     系统提示词（每轮对话）                          │
│                                                                   │
│  1. 情景记忆   ─── 最相关的 6 条历史任务经验                      │
│  2. 记忆上下文 ─── MEMORY.md + 最近 7 天每日笔记                   │
│  3. 实体知识库 ─── 所有已知实体的一行摘要                          │
│                                                                   │
│  工具：save_memory / recall_memory  （实体知识库 CRUD）             │
└──────────────────────────────────────────────────────────────────┘
```

### 第一层：情景记忆（自动）

每个完成的任务会自动记录为一条**情景（episode）**，存储在 `episodes.redb` 嵌入式数据库中。每条情景包含：

- **摘要** — 由 LLM 生成，截断至 500 字符
- **结果** — 成功、失败、阻塞或取消
- **修改的文件** — 任务期间涉及的文件路径列表
- **关键决策** — 执行过程中的重要选择
- **工作目录** — 用于按目录范围检索

每次开始新任务时，智能体会从情景库中检索最多 **6 条相关历史经验**，检索方式为：

- **混合搜索**（配置了向量嵌入时默认）：结合 BM25 关键词匹配（30% 权重）和 HNSW 向量相似度（70% 权重）
- **关键词搜索**（未配置嵌入时的回退）：将查询词与情景摘要进行匹配，限定在同一工作目录范围内

**向量嵌入配置**（在 `config.json` 中）：

```json
{
  "embedding": {
    "provider": "openai",
    "api_key_env": "OPENAI_API_KEY",
    "base_url": null
  }
}
```

配置后，智能体会以"发射后不管"（fire-and-forget）的方式在后台对每条情景摘要生成向量嵌入，并与情景一同存储。查询时，任务指令会被嵌入并用于向量搜索。未配置时，系统回退到纯 BM25 关键词匹配。

### 第二层：长期记忆与每日笔记（基于文件）

**长期记忆**（`.octos/memory/MEMORY.md`）保存跨会话的持久化事实和笔记。可通过手动编辑或 `write_file` 工具写入——其内容会在每轮对话中完整注入系统提示词。

**每日笔记**（`.octos/memory/YYYY-MM-DD.md`）提供近期活动的滚动窗口。最近 **7 天**的每日笔记会自动纳入智能体上下文。这些文件可以手动创建或通过 `write_file` 工具生成。

> **注意：** 每日笔记由系统提示词构建器读取，但不会自动填充内容。你可以手动写入或指示智能体通过 `write_file` 工具写入。

### 第三层：实体知识库（工具驱动）

实体知识库是位于 `.octos/memory/bank/entities/` 的结构化知识存储。每个实体是一个 Markdown 文件，包含智能体对特定主题的所有认知。

**工作原理：**

1. **摘要注入提示词** — 每个实体的第一个非标题行成为一行摘要。所有摘要被注入系统提示词，为智能体提供一个精简的知识索引。
2. **按需加载全文** — 智能体使用 `recall_memory` 工具在需要详情时加载特定实体的完整内容。
3. **智能体自主管理** — 智能体通过 `save_memory` 工具自行决定何时创建和更新实体。

**记忆工具：**

- **`save_memory`** — 创建或更新实体页面。智能体被要求先通过 `recall_memory` 读取已有内容，然后合并新信息再保存（避免数据丢失）。
- **`recall_memory`** — 加载指定实体的完整内容。如果实体不存在，则返回所有可用实体列表。

> **自动延迟：** 当工具总数超过 15 个时，记忆工具会被移入 `group:memory` 延迟组。智能体需先使用 `activate_tools` 启用它们，然后才能保存或回忆知识。

## 文件结构

```
.octos/
├── config.json              # 配置文件（版本化，自动迁移）
├── cron.json                # 定时任务存储
├── AGENTS.md                # 智能体指令
├── SOUL.md                  # 人格设定
├── USER.md                  # 用户信息
├── HEARTBEAT.md             # 后台任务
├── sessions/                # 聊天历史（JSONL）
├── memory/                  # 记忆文件
│   ├── MEMORY.md            # 长期记忆（手动或 write_file）
│   ├── 2025-02-10.md        # 每日笔记（手动或 write_file）
│   └── bank/
│       └── entities/        # 实体知识库（由 save/recall 工具管理）
│           ├── yuechen.md   # 实体：「用户是谁」
│           └── octos.md     # 实体：「这个项目是什么」
├── skills/                  # 自定义技能
├── episodes.redb            # 情景记忆数据库（自动填充）
└── history/
    └── chat_history         # Readline 历史
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
