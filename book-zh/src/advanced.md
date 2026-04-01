# 高级功能

本章介绍面向高级用户的功能：工具管理、队列模式、生命周期钩子、沙箱隔离、会话管理和 Web 仪表板。

---

## 工具与 LRU 延迟加载

Octos 通过将工具分为**活跃**和**延迟**两组来管理庞大的工具目录。活跃工具会作为可调用的工具规格发送给 LLM；延迟工具仅以名称列出在系统提示中，在需要时才发送完整规格。

### 工作原理

- **基础工具**（不会被淘汰）：`read_file`、`write_file`、`shell`、`glob`、`grep`、`list_dir`、`run_pipeline`、`deep_search` 等。
- **动态工具**：`save_memory`、`web_search`、`recall_memory` 等按需激活、空闲后淘汰的工具。
- **延迟工具**：`browser`、`manage_skills`、`spawn`、`configure_tool`、`switch_model` 等仅列出名称的工具。

### 淘汰规则

当活跃工具数量超过 15 个时：
- 空闲 5 次以上迭代且不在基础工具集中的工具成为淘汰候选。
- 最久未使用的工具优先移入延迟列表。

### 重新激活

当 LLM 需要使用某个延迟工具时，它会调用 `activate_tools({"tools": [...]})`。这会将工具名称解析到对应的工具组，并激活整个组。

### 工具配置

可以在运行时通过 `/config` 斜杠命令配置工具。设置持久化存储在 `{data_dir}/tool_config.json` 中。

| 工具 | 设置项 | 类型 | 默认值 | 说明 |
|------|---------|------|---------|-------------|
| `news_digest` | `language` | `"zh"` / `"en"` | `"zh"` | 新闻摘要的输出语言 |
| `news_digest` | `hn_top_stories` | 5-100 | 30 | Hacker News 抓取的故事数 |
| `news_digest` | `max_rss_items` | 5-100 | 30 | 每个 RSS 源的条目数 |
| `news_digest` | `max_deep_fetch_total` | 1-50 | 20 | 深度抓取的文章总数 |
| `news_digest` | `max_source_chars` | 1000-50000 | 12000 | 每个来源的 HTML 字符上限 |
| `news_digest` | `max_article_chars` | 1000-50000 | 8000 | 每篇文章的内容字符上限 |
| `deep_crawl` | `page_settle_ms` | 500-10000 | 3000 | JS 渲染等待时间（毫秒） |
| `deep_crawl` | `max_output_chars` | 10000-200000 | 50000 | 输出截断上限 |
| `web_search` | `count` | 1-10 | 5 | 默认搜索结果数量 |
| `web_fetch` | `extract_mode` | `"markdown"` / `"text"` | `"markdown"` | 内容提取格式 |
| `web_fetch` | `max_chars` | 1000-200000 | 50000 | 内容大小上限 |
| `browser` | `action_timeout_secs` | 30-600 | 300 | 单次操作超时时间 |
| `browser` | `idle_timeout_secs` | 60-600 | 300 | 空闲会话超时时间 |

**聊天中的配置命令：**

```
/config                              # 显示所有工具设置
/config web_search                   # 显示 web_search 的设置
/config set web_search.count 10      # 将默认结果数设为 10
/config set news_digest.language en  # 将新闻摘要切换为英文
/config reset web_search.count       # 重置为默认值
```

**优先级顺序**（从高到低）：
1. 显式的单次调用参数（工具调用时指定的参数）
2. `/config` 覆盖值（存储在 `tool_config.json` 中）
3. 硬编码的默认值

---

## 工具策略

工具策略控制 Agent 可以使用哪些工具，可在全局、按提供商或按上下文级别进行设置。

### 全局策略

```json
{
  "tool_policy": {
    "allow": ["group:fs", "group:search", "web_search"],
    "deny": ["shell", "spawn"]
  }
}
```

- **`allow`** -- 如果非空，则只允许使用这些工具。如果为空，则允许所有工具。
- **`deny`** -- 始终禁止使用这些工具。**deny 优先于 allow。**

### 命名分组

| 分组 | 展开为 |
|-------|-----------|
| `group:fs` | `read_file`、`write_file`、`edit_file`、`diff_edit` |
| `group:runtime` | `shell` |
| `group:web` | `web_search`、`web_fetch`、`browser` |
| `group:search` | `glob`、`grep`、`list_dir` |
| `group:sessions` | `spawn` |

不在命名分组中的工具：`send_file`、`switch_model`、`run_pipeline`、`configure_tool`、`cron`、`message`。

### 通配符匹配

后缀 `*` 匹配前缀：

```json
{
  "tool_policy": {
    "deny": ["web_*"]
  }
}
```

这会禁止 `web_search`、`web_fetch` 等工具。

### 按提供商的策略

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

---

## 队列模式

队列模式控制 Agent 正在处理上一个请求时，新到达的用户消息如何处理。可通过聊天中的 `/queue <mode>` 或配置文件中的 `queue_mode` 设置。

### Followup（默认）

顺序处理。每条消息依次等待处理。

- Agent 处理 A，完成后处理 B，完成后处理 C。
- 简单且可预测。
- 当前请求完成前，用户处于等待状态。

### Collect

将排队的消息批量合并为一个组合提示。

- Agent 正在处理 A。用户发送 B，然后 C。
- A 完成后，B 和 C 合并为一个提示：`B\n---\nQueued #1: C`
- 一次 LLM 调用处理整个批次。
- 适合习惯连续发送多条短消息的用户（在聊天应用中很常见）。

### Steer

只保留最新的排队消息，丢弃较旧的。

- Agent 正在处理 A。用户发送 B，然后 C。
- A 完成后，B 被丢弃；只处理 C。
- 适合用户在等待过程中修正或完善问题的场景。
- 示例："搜索 X" 然后 "还是搜索 Y 吧" -- 只处理 Y。

### Interrupt

只保留最新的排队消息并取消正在运行的 Agent。

- Agent 正在处理 A。用户发送 B，然后 C。
- A 被**取消**，B 被丢弃，C 立即开始处理。
- 对方向修正的响应最快。
- 当响应速度比完成当前任务更重要时使用。

> **注意：** 当前 Interrupt 和 Steer 共享相同的排空并丢弃行为。不存在飞行中的 Agent 取消——正在运行的 Agent 会在处理最新消息之前完成。真正的飞行中取消功能正在计划中。

### Speculative

为每条新消息生成并发的溢出 Agent，同时主 Agent 继续运行。

- Agent 正在处理 A。用户发送 B，然后 C。
- B 和 C 各自获得独立的并发 Agent 任务（溢出）。
- 三者并行运行 -- 无阻塞。
- 最适合 LLM 提供商响应较慢、用户不想等待的场景。
- 溢出 Agent 使用主 Agent 启动前的会话历史快照。

#### 溢出机制的工作原理

1. 为第一条消息生成主 Agent。
2. 主 Agent 运行期间，新消息到达收件箱。
3. 每条新消息触发 `serve_overflow()`，生成一个拥有独立流式输出气泡的完整 Agent 任务。
4. 溢出 Agent 使用主 Agent 启动前的历史快照，避免重复回答主问题。
5. 所有 Agent 并发运行，结果保存到会话历史中。

#### 已知限制

- **交互式提示在溢出中无法正常工作**：如果 LLM 提出后续问题并返回 EndTurn，溢出 Agent 会退出。用户的回复会生成一个新的溢出，但没有上下文来理解之前的问题。
- **短回复可能被误分类**："是"或"2"这样的继续确认可能被当作独立的新查询处理。

### 自动升级

当检测到持续的延迟恶化时，会话 actor 可以自动从 Followup 升级到 Speculative：

- `ResponsivenessObserver` 从前 5 次请求中学习**中位数**基线（对异常值更鲁棒），然后在 20 样本的滚动窗口中跟踪 LLM 响应时间。基线每 20 个样本通过 80/20 EMA 混合当前窗口中位数进行**自适应调整**，可跟踪渐进漂移。
- 如果连续 3 次响应超过 **3×基线** 延迟，同时自动激活 Speculative 队列模式和对冲竞速（Hedge）。
- 发送用户通知："检测到响应缓慢，已启用对冲竞速 + 投机队列。"
- 当提供商恢复（一次正常延迟的响应）时，两者都恢复为 Followup 和静态路由。
- API 通道（Web 客户端）也会触发自动升级，因为它始终使用投机处理路径。

### 队列命令

```
/queue                  -- 显示当前模式
/queue followup         -- 顺序处理
/queue collect          -- 批量合并排队消息
/queue steer            -- 只保留最新消息
/queue interrupt        -- 取消当前任务 + 保留最新消息
/queue speculative      -- 并发溢出 Agent
```

---

## 钩子

钩子是用于执行 LLM 策略、记录指标和审计 Agent 行为的主要扩展点 -- 按配置文件定义，无需修改核心代码。

钩子是在 Agent 生命周期事件触发时运行的 shell 命令。每个钩子通过 stdin 接收 JSON 载荷，通过退出码传达决策。

### 退出码

| 退出码 | 含义 | Before 事件 | After 事件 |
|-----------|---------|---------------|--------------|
| 0 | 允许 | 操作继续执行 | 记录为成功 |
| 1 | 拒绝 | 操作被阻止（原因输出到 stdout） | 视为错误 |
| 2+ | 错误 | 记录日志，操作继续执行 | 记录日志 |

### 事件

四个生命周期事件，每个都有特定的载荷：

#### `before_tool_call`

在每次工具执行前触发。**可以拒绝**（exit 1）。

```json
{
  "event": "before_tool_call",
  "tool_name": "shell",
  "arguments": {"command": "ls -la"},
  "tool_id": "call_abc123",
  "session_id": "telegram:12345",
  "profile_id": "my-bot"
}
```

#### `after_tool_call`

在每次工具执行后触发。仅用于观测。

```json
{
  "event": "after_tool_call",
  "tool_name": "shell",
  "tool_id": "call_abc123",
  "result": "file1.txt\nfile2.txt\n...",
  "success": true,
  "duration_ms": 142,
  "session_id": "telegram:12345",
  "profile_id": "my-bot"
}
```

注意：`result` 被截断到 500 个字符。

#### `before_llm_call`

在每次 LLM API 调用前触发。**可以拒绝**（exit 1）。

```json
{
  "event": "before_llm_call",
  "model": "deepseek-chat",
  "message_count": 12,
  "iteration": 3,
  "session_id": "telegram:12345",
  "profile_id": "my-bot"
}
```

#### `after_llm_call`

在每次成功的 LLM 响应后触发。仅用于观测。

```json
{
  "event": "after_llm_call",
  "model": "deepseek-chat",
  "iteration": 3,
  "stop_reason": "EndTurn",
  "has_tool_calls": false,
  "input_tokens": 1200,
  "output_tokens": 350,
  "provider_name": "deepseek",
  "latency_ms": 2340,
  "cumulative_input_tokens": 5600,
  "cumulative_output_tokens": 1800,
  "session_cost": 0.0042,
  "response_cost": 0.0012,
  "session_id": "telegram:12345",
  "profile_id": "my-bot"
}
```

### 钩子配置

在 `config.json` 或按配置文件的 JSON 中：

```json
{
  "hooks": [
    {
      "event": "before_tool_call",
      "command": ["python3", "~/.octos/hooks/guard.py"],
      "timeout_ms": 3000,
      "tool_filter": ["shell", "write_file"]
    },
    {
      "event": "after_llm_call",
      "command": ["python3", "~/.octos/hooks/cost-tracker.py"],
      "timeout_ms": 5000
    }
  ]
}
```

| 字段 | 必填 | 默认值 | 说明 |
|-------|----------|---------|-------------|
| `event` | 是 | -- | 四种事件类型之一 |
| `command` | 是 | -- | argv 数组（不经过 shell 解释） |
| `timeout_ms` | 否 | 5000 | 超时后终止钩子进程 |
| `tool_filter` | 否 | 全部 | 仅对这些工具名称触发（仅限工具事件） |

同一事件可以注册多个钩子。它们按顺序执行；第一个拒绝即生效。

### 熔断器

钩子在连续 3 次失败（超时、崩溃或退出码 2+）后会被自动禁用。一次成功执行（exit 0 或拒绝 exit 1）即可重置计数器。

### 安全性

- 命令使用 argv 数组 -- 不经过 shell 解释。
- 18 个危险环境变量会被移除（`LD_PRELOAD`、`DYLD_*`、`NODE_OPTIONS` 等）。
- 支持波浪号展开（`~/` 和 `~username/`）。

### 按配置文件的钩子

每个配置文件可以通过配置中的 `hooks` 字段定义自己的钩子。这允许不同的频道或机器人使用不同的策略。钩子变更需要重启网关。

### 向后兼容性

- 载荷中可能会添加新字段。
- 现有字段永远不会被移除或重命名。
- 钩子脚本应忽略未知字段（标准 JSON 实践）。

### 示例：费用预算控制器

```python
#!/usr/bin/env python3
"""Deny LLM calls when session cost exceeds $1.00."""
import json, sys

payload = json.load(sys.stdin)
if payload.get("event") == "before_llm_call":
    try:
        with open("/tmp/octos-cost.json") as f:
            state = json.load(f)
    except FileNotFoundError:
        state = {}
    sid = payload.get("session_id", "default")
    if state.get(sid, 0) > 1.0:
        print(f"Session cost exceeded $1.00 (${state[sid]:.4f})")
        sys.exit(1)

elif payload.get("event") == "after_llm_call":
    cost = payload.get("session_cost")
    if cost is not None:
        sid = payload.get("session_id", "default")
        try:
            with open("/tmp/octos-cost.json") as f:
                state = json.load(f)
        except FileNotFoundError:
            state = {}
        state[sid] = cost
        with open("/tmp/octos-cost.json", "w") as f:
            json.dump(state, f)

sys.exit(0)
```

### 示例：审计日志记录器

```python
#!/usr/bin/env python3
"""Log all tool and LLM calls to a JSONL file."""
import json, sys, datetime

payload = json.load(sys.stdin)
payload["timestamp"] = datetime.datetime.utcnow().isoformat()

with open("/var/log/octos-audit.jsonl", "a") as f:
    f.write(json.dumps(payload) + "\n")

sys.exit(0)
```

---

## 沙箱

Shell 命令在沙箱中运行以实现隔离。支持三种后端：

| 后端 | 平台 | 隔离方式 | 网络控制 |
|---------|----------|-------|---------|
| bwrap | Linux | 只读绑定 `/usr,/lib,/bin,/sbin,/etc`；读写绑定工作目录；tmpfs `/tmp`；unshare-pid | 禁止网络时使用 `--unshare-net` |
| macOS | macOS | 使用 SBPL 配置的 sandbox-exec：`process-exec/fork`、`file-read*`、工作目录 + `/private/tmp` 写入 | `(allow network*)` 或 `(deny network*)` |
| Docker | 任意平台 | `--rm --security-opt no-new-privileges --cap-drop ALL` | 禁止网络时使用 `--network none` |

在 `config.json` 中配置：

```json
{
  "sandbox": {
    "enabled": true,
    "mode": "auto",
    "allow_network": false,
    "docker": {
      "image": "alpine:3.21",
      "mount_mode": "rw",
      "cpu_limit": "1.0",
      "memory_limit": "512m",
      "pids_limit": 100
    }
  }
}
```

- **模式**：`auto`（自动检测最佳可用方案）、`bwrap`、`macos`、`docker`、`none`。
- **挂载模式**：`rw`（读写）、`ro`（只读）、`none`（不挂载工作区）。
- **Docker 资源限制**：`--cpus`、`--memory`、`--pids-limit`。
- **Docker 绑定挂载安全**：`docker.sock`、`/proc`、`/sys`、`/dev` 和 `/etc` 被阻止作为绑定挂载源。
- **路径验证**：Docker 拒绝 `:`、`\0`、`\n`、`\r`；macOS 拒绝控制字符、`(`、`)`、`\`、`"`。
- **环境变量清理**：18 个危险环境变量在所有沙箱后端、MCP 服务器启动、钩子和浏览器工具中自动清除：`LD_PRELOAD, LD_LIBRARY_PATH, LD_AUDIT, DYLD_INSERT_LIBRARIES, DYLD_LIBRARY_PATH, DYLD_FRAMEWORK_PATH, DYLD_FALLBACK_LIBRARY_PATH, DYLD_VERSIONED_LIBRARY_PATH, NODE_OPTIONS, PYTHONSTARTUP, PYTHONPATH, PERL5OPT, RUBYOPT, RUBYLIB, JAVA_TOOL_OPTIONS, BASH_ENV, ENV, ZDOTDIR`。
- **进程清理**：Shell 工具在超时时发送 SIGTERM，等待宽限期后发送 SIGKILL 清理子进程。

---

## 会话管理

### 会话分支

发送 `/new` 创建一个分支对话：

```
/new
```

这会创建一个新会话，复制当前对话的最近 10 条消息。子会话通过 `parent_key` 引用原始会话。每个分支获得一个由发送者和时间戳组成的唯一键。

### 会话持久化

每个 channel:chat_id 对维护各自独立的会话（对话历史）。

- **存储**：`.octos/sessions/` 中的 JSONL 文件
- **最大历史**：通过 `gateway.max_history` 配置（默认：50 条消息）
- **会话分支**：`/new` 创建带有 parent_key 追踪的分支对话

### 配置热重载

网关自动检测配置文件变更：

- **可热重载**（无需重启）：系统提示、AGENTS.md、SOUL.md、USER.md
- **需要重启**：提供商、模型、API 密钥、网关频道

变更通过 SHA-256 哈希和防抖机制检测。

### 消息合并

长响应在发送前会自动拆分为适合频道的分块：

| 频道 | 每条消息最大字符数 |
|---------|-----------------------|
| Telegram | 4000 |
| Discord | 1900 |
| Slack | 3900 |

拆分优先级：段落分隔 > 换行符 > 句号结尾 > 空格 > 硬截断。超过 50 块的消息会被截断并添加标记。

---

## 上下文压缩

当对话超出 LLM 的上下文窗口时，较旧的消息会被自动压缩：

- 工具参数被剥离（替换为 `"[stripped]"`）
- 消息被摘要为首行内容
- 最近的工具调用/结果对完整保留
- Agent 无缝继续，不会丢失关键上下文

---

## 聊天内命令

### 斜杠命令

| 命令 | 说明 |
|---------|-------------|
| `/new` | 分支对话（创建复制最近 10 条消息的新会话） |
| `/config` | 查看和修改工具配置 |
| `/queue` | 查看或更改队列模式 |
| `/exit`、`/quit`、`:q` | 退出聊天（仅 CLI 模式） |

### 聊天中切换提供商

`switch_model` 工具允许用户通过自然对话列出可用的 LLM 提供商并在运行时切换模型。此工具仅在网关模式下可用。

**列出可用提供商：**

```
User: What models are available?

Bot: Current model: deepseek/deepseek-chat

     Available providers:
       - anthropic (default: claude-sonnet-4-20250514) [ready]
       - openai (default: gpt-4o) [ready]
       - deepseek (default: deepseek-chat) [ready]
       - gemini (default: gemini-2.0-flash) [ready]
       ...
```

**切换模型：**

```
User: Switch to GPT-4o

Bot: Switched to openai/gpt-4o.
     Previous model (deepseek/deepseek-chat) is kept as fallback.
```

切换模型时，之前的模型自动成为备选：
- 如果新模型失败（限流、服务器错误），请求自动回退到原始模型。
- 回退使用熔断器机制（连续 3 次失败触发故障转移）。
- 链式结构始终扁平：`[new_model, original_model]` -- 反复切换不会嵌套。

模型切换持久化到配置文件 JSON。网关重启时，机器人以最后选择的模型启动。

### 记忆系统

Agent 在会话间维护长期记忆：

- **`MEMORY.md`** -- 持久化笔记，始终加载到上下文中
- **每日笔记** -- `.octos/memory/YYYY-MM-DD.md`，自动创建
- **近期记忆** -- 最近 7 天的每日笔记包含在上下文中
- **片段记忆** -- 任务完成摘要存储在 `episodes.redb` 中

### 混合记忆搜索

记忆搜索结合了 BM25（关键词）和向量（语义）评分：

- **排名**：`alpha * vector_score + (1 - alpha) * bm25_score`（默认 alpha：0.7）
- **索引**：使用 L2 归一化嵌入的 HNSW
- **降级方案**：未配置嵌入提供商时仅使用 BM25

配置嵌入提供商以启用向量搜索：

```json
{
  "embedding": {
    "provider": "openai"
  }
}
```

嵌入配置支持三个字段：`provider`（默认：`"openai"`）、`api_key_env`（可选覆盖）和 `base_url`（可选自定义端点）。

### 定时任务（Cron Jobs）

Agent 可以使用 `cron` 工具调度周期性任务：

```
User: Schedule a daily news digest at 8am Beijing time

Bot: Created cron job "daily-news" running at 8:00 AM Asia/Shanghai every day.
     Expression: 0 0 8 * * * *
```

定时任务也可以通过 CLI 管理：

```bash
octos cron list                              # 列出活跃任务
octos cron list --all                        # 包含已禁用的
octos cron add --name "report" --message "Generate daily report" --cron "0 0 9 * * * *"
octos cron add --name "check" --message "Check status" --every 3600
octos cron remove <job-id>
octos cron enable <job-id>
octos cron enable <job-id> --disable
```

---

## Web 仪表板

REST API 服务器包含一个内嵌的 Web 界面：

```bash
octos serve                              # 绑定到 127.0.0.1:8080
octos serve --host 0.0.0.0 --port 3000  # 接受外部连接
# 打开 http://localhost:8080
```

功能：
- 会话侧边栏
- 聊天界面
- SSE 流式推送
- 暗色主题

`/metrics` 端点提供 Prometheus 格式的指标：
- `octos_tool_calls_total`
- `octos_tool_call_duration_seconds`
- `octos_llm_tokens_total`
