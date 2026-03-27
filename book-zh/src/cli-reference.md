# CLI 命令参考

## `octos chat`

交互式多轮对话，支持 readline 历史记录。

```
octos chat [OPTIONS]

Options:
  -c, --cwd <PATH>         工作目录
      --config <PATH>      配置文件路径
      --provider <NAME>    LLM 供应商
      --model <NAME>       模型名称
      --base-url <URL>     自定义 API 端点
  -m, --message <MSG>      单条消息（非交互模式）
      --max-iterations <N> 每条消息的最大工具迭代次数（默认：50）
  -v, --verbose            显示工具输出
      --no-retry           禁用重试
```

**功能特性：**

- 方向键和行编辑（rustyline）
- 持久化历史记录，保存在 `.octos/history/chat_history`
- 退出方式：`/exit`、`/quit`、`exit`、`quit`、`:q`、Ctrl+C、Ctrl+D
- 完整工具访问（Shell、文件、搜索、Web）

**示例：**

```bash
octos chat                              # 交互模式（默认）
octos chat --provider deepseek          # 使用 DeepSeek
octos chat --model glm-4-plus           # 自动识别为智谱
octos chat --message "Fix auth bug"     # 单条消息，执行后退出
```

---

## `octos gateway`

以常驻多渠道守护进程方式运行。

```
octos gateway [OPTIONS]

Options:
  -c, --cwd <PATH>         工作目录
      --config <PATH>      配置文件路径
      --provider <NAME>    覆盖供应商
      --model <NAME>       覆盖模型
      --base-url <URL>     覆盖 API 端点
  -v, --verbose            详细日志
      --no-retry           禁用重试
```

需要在配置文件中包含 `gateway` 部分及 `channels` 数组。持续运行直至按下 Ctrl+C。

---

## `octos init`

初始化工作区，创建配置和引导文件。

```
octos init [OPTIONS]

Options:
  -c, --cwd <PATH>    工作目录
      --defaults       跳过交互提示，使用默认值
```

**创建内容：**

- `.octos/config.json` -- 供应商/模型配置
- `.octos/.gitignore` -- 忽略状态文件
- `.octos/AGENTS.md` -- 智能体指令模板
- `.octos/SOUL.md` -- 个性模板
- `.octos/USER.md` -- 用户信息模板
- `.octos/memory/` -- 记忆存储目录
- `.octos/sessions/` -- 会话历史目录
- `.octos/skills/` -- 自定义技能目录

---

## `octos status`

显示系统状态。

```
octos status [OPTIONS]

Options:
  -c, --cwd <PATH>    工作目录
```

**输出示例：**

```
octos Status
══════════════════════════════════════════════════

Config:    .octos/config.json (found)
Workspace: .octos/            (found)
Provider:  anthropic
Model:     claude-sonnet-4-20250514

API Keys
──────────────────────────────────────────────────
  Anthropic    ANTHROPIC_API_KEY         set
  OpenAI       OPENAI_API_KEY           not set
  ...

Bootstrap Files
──────────────────────────────────────────────────
  AGENTS.md        found
  SOUL.md          found
  USER.md          found
  TOOLS.md         missing
  IDENTITY.md      missing
```

---

## `octos serve`

启动 Web 界面和 REST API 服务器。需要在编译时启用 `api` 特性。

```bash
cargo install --path crates/octos-cli --features api
octos serve                              # 绑定到 127.0.0.1:8080
octos serve --host 0.0.0.0 --port 3000  # 接受外部连接
```

功能包括：会话侧栏、聊天界面、SSE 流式传输、暗色主题。`/metrics` 端点提供 Prometheus 格式的指标（`octos_tool_calls_total`、`octos_tool_call_duration_seconds`、`octos_llm_tokens_total`）。

---

## `octos clean`

清理数据库和状态文件。

```bash
octos clean [--all] [--dry-run]
```

| 参数 | 说明 |
|------|------|
| `--all` | 移除所有状态文件 |
| `--dry-run` | 仅显示将被删除的内容，不实际执行 |

---

## `octos completions`

生成 Shell 自动补全脚本。

```bash
octos completions <shell>
```

支持的 Shell：`bash`、`zsh`、`fish`、`powershell`。

---

## `octos cron`

管理定时任务。

```bash
octos cron list [--all]                  # 列出活跃任务（--all 包含已禁用的）
octos cron add [OPTIONS]                 # 添加定时任务
octos cron remove <job-id>               # 移除定时任务
octos cron enable <job-id>               # 启用定时任务
octos cron enable <job-id> --disable     # 禁用定时任务
```

**添加任务：**

```bash
octos cron add --name "report" --message "Generate daily report" --cron "0 0 9 * * * *"
octos cron add --name "check" --message "Check status" --every 3600
octos cron add --name "once" --message "Run migration" --at "2025-03-01T09:00:00Z"
```

Cron 表达式使用标准语法。任务支持可选的 `timezone` 字段，使用 IANA 时区名称（如 `"America/New_York"`、`"Asia/Shanghai"`）。未指定时默认使用 UTC。

---

## `octos channels`

管理消息渠道。

```bash
octos channels status    # 显示渠道的编译/配置状态
octos channels login     # WhatsApp 二维码登录
```

status 命令会显示一张表格，包含渠道名称、编译状态（特性标志）和配置摘要（环境变量的设置/缺失情况）。

---

## `octos office`

Office 文件操作（DOCX/PPTX/XLSX）。基本操作使用原生 Rust 实现，无需外部依赖。

```bash
octos office extract <file>               # 提取文本为 Markdown
octos office unpack <file> <output-dir>   # 解包为格式化的 XML
octos office pack <input-dir> <output>    # 将目录打包为 Office 文件
octos office clean <dir>                  # 清理解包后 PPTX 中的孤立文件
```

---

## `octos account`

管理 Profile 下的子账户。子账户继承 LLM 供应商配置，但拥有独立的数据目录（记忆、会话、技能）和渠道。

```bash
octos account list --profile <id>                         # 列出子账户
octos account create --profile <id> <name> [OPTIONS]      # 创建子账户
octos account update <id> [OPTIONS]                       # 更新子账户
```

---

## `octos auth`

OAuth 登录和 API 密钥管理。

```bash
octos auth login --provider openai           # PKCE 浏览器 OAuth
octos auth login --provider openai --device-code  # 设备码流程
octos auth login --provider anthropic        # 粘贴令牌（标准输入）
octos auth logout --provider openai          # 移除已存储的凭据
octos auth status                            # 显示已认证的供应商
```

凭据存储在 `~/.octos/auth.json`（文件权限 0600）。解析 API 密钥时，优先检查凭据存储，其次才是环境变量。

---

## `octos skills`

管理技能。

```bash
octos skills list                            # 列出已安装的技能
octos skills install user/repo/skill-name    # 从 GitHub 安装
octos skills remove skill-name               # 移除技能
```

从 GitHub 仓库的 main 分支获取 `SKILL.md` 并安装到 `.octos/skills/`。
