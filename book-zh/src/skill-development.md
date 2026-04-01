# Octos 应用技能开发指南

[English](app-skill-dev-guide.md) | [中文](app-skill-dev-guide-zh.md)

本指南涵盖了构建、注册和部署 octos 应用技能所需的全部内容。

---

## 架构概览

应用技能是一个**独立的可执行二进制文件**，通过简单的 **stdin/stdout JSON 协议**与 octos 网关通信。网关为每次工具调用将技能二进制文件作为子进程启动，通过 stdin 传递 JSON 参数，并从 stdout 读取 JSON 结果。

```
User message → LLM → tool_use("get_weather", {"city": "Paris"})
                         ↓
              Gateway spawns: ~/.octos/skills/weather/main get_weather
                         ↓
              Stdin:  {"city": "Paris"}
              Stdout: {"output": "Paris, France\nClear sky\n...", "success": true}
                         ↓
              LLM sees result → generates natural language response
```

---

## 技能目录结构

每个技能在 `crates/app-skills/` 下有自己独立的 crate：

```
crates/app-skills/my-skill/
├── Cargo.toml          # Crate 配置，二进制名称
├── manifest.json       # 工具定义（JSON Schema）
├── SKILL.md            # 文档 + frontmatter 元数据
└── src/
    └── main.rs         # 二进制入口
```

启动引导后，技能安装在：

```
~/.octos/skills/my-skill/
├── main                # 可执行二进制文件（从 target/ 复制）
├── manifest.json       # 工具定义
└── SKILL.md            # 文档
```

---

## 分步指南：创建新技能

### 1. 创建 Crate

```bash
mkdir -p crates/app-skills/my-skill/src
```

### 2. Cargo.toml

```toml
[package]
name = "my-skill"
version = "1.0.0"
edition = "2021"
description = "Short description of what this skill does"
authors = ["your-name"]

[[bin]]
name = "my_skill"          # Binary name (used in bundled_app_skills.rs)
path = "src/main.rs"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
# Add other deps as needed:
# reqwest = { version = "0.12", features = ["blocking", "rustls-tls", "json"], default-features = false }
# chrono = "0.4"
```

**重要：** `[[bin]] name` 必须与 `bundled_app_skills.rs` 中的 `binary_name` 匹配。

### 3. manifest.json

定义 LLM 可调用的工具。使用 JSON Schema 进行输入验证。

```json
{
  "name": "my-skill",
  "version": "1.0.0",
  "author": "your-name",
  "description": "What this skill does",
  "timeout_secs": 15,
  "requires_network": false,
  "tools": [
    {
      "name": "my_tool",
      "description": "Clear description for the LLM. What does this tool do? When should it be used?",
      "input_schema": {
        "type": "object",
        "properties": {
          "param1": {
            "type": "string",
            "description": "What this parameter means"
          },
          "param2": {
            "type": "integer",
            "description": "Optional numeric parameter (default: 10)"
          }
        },
        "required": ["param1"]
      }
    }
  ]
}
```

**清单字段：**

| 字段 | 必填 | 默认值 | 说明 |
|-------|----------|---------|-------------|
| `name` | 是 | — | 技能标识符 |
| `version` | 是 | — | 语义化版本 |
| `author` | 否 | — | 作者名称 |
| `description` | 否 | — | 可读的描述 |
| `timeout_secs` | 否 | 30 | 每次工具调用的最大执行时间（1-600） |
| `requires_network` | 否 | false | 信息性标志 |
| `sha256` | 否 | — | 二进制完整性校验（十六进制哈希） |
| `tools` | 是 | — | 工具定义数组 |

**工具定义字段：**

| 字段 | 必填 | 说明 |
|-------|----------|-------------|
| `name` | 是 | 工具名称（snake_case，全局唯一） |
| `description` | 是 | 展示给 LLM 的描述 -- 明确说明何时使用 |
| `input_schema` | 是 | 输入参数的 JSON Schema |

### 4. SKILL.md

带有 YAML frontmatter 的文档。LLM 通过阅读它来理解何时以及如何使用该技能。

```markdown
---
name: my-skill
description: Short description. Triggers: keyword1, keyword2, 关键词, trigger phrase.
version: 1.0.0
author: your-name
always: false
---

# My Skill

Detailed description of what this skill does and when to use it.

## Tools

### my_tool

Explain what this tool does with examples.

\```json
{"param1": "example value", "param2": 5}
\```

**Parameters:**
- `param1` (required): What it means
- `param2` (optional): What it controls. Default: 10
```

**Frontmatter 字段：**

| 字段 | 必填 | 默认值 | 说明 |
|-------|----------|---------|-------------|
| `name` | 是 | — | 技能标识符 |
| `description` | 是 | — | 一行描述。在 "Triggers:" 后面添加触发关键词 |
| `version` | 是 | — | 语义化版本 |
| `author` | 否 | — | 作者名称 |
| `always` | 否 | `false` | 如果为 `true`，技能文档始终包含在系统提示中 |
| `requires_bins` | 否 | — | 逗号分隔的二进制文件列表（通过 `which` 检查是否存在） |
| `requires_env` | 否 | — | 逗号分隔的环境变量列表（必须已设置） |

**触发关键词**帮助 Agent 决定何时激活该技能。如果用户使用多种语言，请包含多语言的触发词。

### 5. src/main.rs

二进制文件实现 stdin/stdout 协议。

**最小模板：**

```rust
use std::io::Read;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct MyToolInput {
    param1: String,
    #[serde(default = "default_param2")]
    param2: i32,
}

fn default_param2() -> i32 { 10 }

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let tool_name = args.get(1).map(|s| s.as_str()).unwrap_or("unknown");

    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        fail(&format!("Failed to read stdin: {e}"));
    }

    match tool_name {
        "my_tool" => handle_my_tool(&buf),
        _ => fail(&format!("Unknown tool '{tool_name}'. Expected: my_tool")),
    }
}

fn fail(msg: &str) -> ! {
    println!("{}", json!({"output": msg, "success": false}));
    std::process::exit(1);
}

fn handle_my_tool(input_json: &str) {
    let input: MyToolInput = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(e) => fail(&format!("Invalid input: {e}")),
    };

    // ... your logic here ...

    let result = format!("Processed {} with param2={}", input.param1, input.param2);
    println!("{}", json!({"output": result, "success": true}));
}
```

**协议规则：**

1. **argv[1]** = 工具名称（例如 `get_weather`、`get_forecast`）
2. **stdin** = 匹配工具 `input_schema` 的 JSON 对象
3. **stdout** = JSON 对象，包含：
   - `output`（字符串）：人类可读的结果文本
   - `success`（布尔值）：成功为 `true`，失败为 `false`
4. **退出码**：成功为 0，失败为非零
5. **stderr**：网关会忽略（可用于调试日志）

---

## 注册技能

### 6. 添加到工作区

在根目录的 `Cargo.toml` 中添加到 `members`：

```toml
[workspace]
members = [
    # ... existing members ...
    "crates/app-skills/my-skill",
]
```

### 7. 在 bundled_app_skills.rs 中注册

在 `crates/octos-agent/src/bundled_app_skills.rs` 中添加到 `BUNDLED_APP_SKILLS`：

```rust
pub const BUNDLED_APP_SKILLS: &[(&str, &str, &str, &str)] = &[
    // ... existing skills ...
    (
        "my-skill",                                          // dir_name (skill directory name)
        "my_skill",                                          // binary_name (must match [[bin]] name)
        include_str!("../../app-skills/my-skill/SKILL.md"),  // embedded docs
        include_str!("../../app-skills/my-skill/manifest.json"), // embedded manifest
    ),
];
```

**元组格式：** `(dir_name, binary_name, skill_md, manifest_json)`

- `dir_name`：`~/.octos/skills/` 下的目录名
- `binary_name`：`target/release/` 中的二进制文件名（必须与 Cargo.toml 中的 `[[bin]] name` 匹配）
- `skill_md`：嵌入的 SKILL.md 内容
- `manifest_json`：嵌入的 manifest.json 内容

---

## 构建与测试

### 8. 构建

```bash
# 只构建你的技能
cargo build -p my-skill

# 构建全部
cargo build --workspace
```

### 9. 独立测试

```bash
# 直接测试你的工具
echo '{"param1": "hello", "param2": 5}' | ./target/debug/my_skill my_tool

# 预期输出：
# {"output":"Processed hello with param2=5","success":true}

# 测试错误处理
echo '{}' | ./target/debug/my_skill my_tool
echo '{"param1": "test"}' | ./target/debug/my_skill unknown_tool
```

### 10. 网关集成测试

```bash
# 构建 release 版本并安装
cargo build --release --workspace

# 启动网关（技能自动引导加载）
octos gateway

# 检查技能是否已加载
ls ~/.octos/skills/my-skill/
# main  manifest.json  SKILL.md

# 让 Agent 使用你的技能
```

---

## 示例

### 示例 1：纯本地技能（时钟）

不需要网络，不需要环境变量。使用 `chrono` + `chrono-tz`。

```
crates/app-skills/time/
├── Cargo.toml          # deps: chrono, chrono-tz, serde, serde_json
├── manifest.json       # 1 tool: get_time, timeout_secs: 5
├── SKILL.md            # Triggers: time, clock, 几点
└── src/main.rs         # Reads system clock, formats with timezone
```

**关键模式：** 未指定时区时默认使用本地时间。

### 示例 2：网络技能（天气）

调用外部 API，需要网络。使用 `reqwest`（blocking）。

```
crates/app-skills/weather/
├── Cargo.toml          # deps: reqwest (blocking, rustls-tls), serde, serde_json
├── manifest.json       # 2 tools: get_weather, get_forecast, timeout_secs: 15
├── SKILL.md            # Triggers: weather, forecast, 天气
└── src/main.rs         # Geocode city → fetch weather from Open-Meteo
```

**关键模式：**
- 构建带超时的 HTTP 客户端
- 优雅处理 API 错误（返回 `success: false`）
- 对用户输入进行 URL 编码
- 一个二进制文件中包含多个工具（根据 `argv[1]` 匹配）

### 示例 3：需要环境变量的技能（发送邮件）

需要从环境变量获取凭据。

```
crates/app-skills/send-email/
├── Cargo.toml          # deps: lettre, serde, serde_json, reqwest
├── manifest.json       # 1 tool: send_email
├── SKILL.md            # requires_env: SMTP_HOST,SMTP_USERNAME,SMTP_PASSWORD
└── src/main.rs         # Reads SMTP_* env vars, sends via SMTP
```

**关键模式：** 尽早检查环境变量，用清晰的错误消息报错。

```rust
fn get_smtp_config() -> SmtpConfig {
    let host = std::env::var("SMTP_HOST")
        .unwrap_or_else(|_| fail("SMTP_HOST env var not set"));
    // ...
}
```

---

## 清单扩展：MCP 服务器、钩子和提示片段

技能可以在 `manifest.json` 中声明的不仅仅是工具。三个额外的扩展点允许技能提供 MCP 服务器、生命周期钩子和系统提示内容。这些统称为 **extras**。

### MCP 服务器

技能可以声明 MCP（Model Context Protocol）服务器，网关在技能加载时自动启动。这让技能可以通过 MCP 协议（而非或同时使用 stdin/stdout 二进制协议）暴露工具。

在 `manifest.json` 中添加 `mcp_servers` 数组：

```json
{
  "name": "my-skill",
  "version": "1.0.0",
  "tools": [],
  "mcp_servers": [
    {
      "command": "node",
      "args": ["mcp-server/index.js"],
      "env": ["API_KEY", "API_SECRET"]
    }
  ]
}
```

**MCP 服务器字段：**

| 字段 | 必填 | 说明 |
|-------|----------|-------------|
| `command` | 否* | 启动 MCP 服务器进程的命令 |
| `args` | 否 | 传递给命令的参数 |
| `env` | 否 | 要转发的环境变量**名称**列表（非值） |
| `url` | 否* | HTTP 传输：远程 MCP 服务器端点的 URL |
| `headers` | 否 | HTTP 传输：附加请求头（键值对象） |

\* `command` 和 `url` 必须设置其中一个。本地（stdio）MCP 服务器使用 `command`，远程（HTTP）MCP 服务器使用 `url`。

**路径解析：** 如果 `command` 以 `./` 或 `../` 开头，则相对于技能目录解析。裸命令（如 `"node"`、`"python3"`）照常从 `PATH` 查找。

**环境变量转发：** `env` 数组包含环境变量的*名称*而非值。加载时，每个名称从进程环境中查找。只有实际设置的变量才会转发给 MCP 服务器进程。缺少的变量会被静默忽略。

**示例：本地 stdio MCP 服务器**

```json
{
  "mcp_servers": [
    {
      "command": "./bin/mcp-server",
      "args": ["--port", "0"],
      "env": ["DATABASE_URL"]
    }
  ]
}
```

**示例：远程 HTTP MCP 服务器**

```json
{
  "mcp_servers": [
    {
      "url": "https://mcp.example.com/v1",
      "headers": {
        "Authorization": "Bearer ${API_KEY}"
      }
    }
  ]
}
```

---

### 生命周期钩子

技能可以声明生命周期钩子，在特定的 Agent 事件发生时运行 shell 命令。这适用于审计、策略执行或副作用。

在 `manifest.json` 中添加 `hooks` 数组：

```json
{
  "name": "my-audit-skill",
  "version": "1.0.0",
  "tools": [],
  "hooks": [
    {
      "event": "after_tool_call",
      "command": ["./hooks/audit.sh"],
      "timeout_ms": 5000,
      "tool_filter": ["shell"]
    }
  ]
}
```

**钩子字段：**

| 字段 | 必填 | 默认值 | 说明 |
|-------|----------|---------|-------------|
| `event` | 是 | -- | 生命周期事件名称（见下表） |
| `command` | 是 | -- | 作为 argv 数组的命令（不经过 shell 解释） |
| `timeout_ms` | 否 | 5000 | 最大执行时间（毫秒） |
| `tool_filter` | 否 | `[]`（所有工具） | 仅对这些工具名称触发（仅限工具事件） |

**支持的事件：**

| 事件 | 可拒绝？ | 触发时机 |
|-------|-----------|---------------|
| `before_tool_call` | 是 | 工具执行前。退出码 1 = 拒绝。 |
| `after_tool_call` | 否 | 工具完成后（无论成功或失败）。 |
| `before_llm_call` | 是 | 向 LLM 发送请求前。退出码 1 = 拒绝。 |
| `after_llm_call` | 否 | 收到 LLM 响应后。 |

**路径解析：** `command` 数组的第一个元素（`command[0]`）遵循与 MCP 服务器相同的规则 -- 以 `./` 或 `../` 开头的路径相对于技能目录解析。其他元素原样传递。

**钩子载荷：** 网关通过 stdin 向钩子进程发送 JSON 载荷。工具事件的载荷包含 `tool_name`、`arguments` 和会话上下文。LLM 事件的载荷包含 `model`、`message_count` 等。

**拒绝行为：** `before_*` 钩子可以通过退出码 1 拒绝操作。钩子的 stdout 内容作为拒绝原因。

**示例：审计所有 shell 工具调用**

```json
{
  "hooks": [
    {
      "event": "before_tool_call",
      "command": ["./hooks/policy-check.sh"],
      "timeout_ms": 3000,
      "tool_filter": ["shell", "bash"]
    },
    {
      "event": "after_tool_call",
      "command": ["./hooks/audit-log.sh"],
      "timeout_ms": 5000,
      "tool_filter": ["shell", "bash"]
    }
  ]
}
```

---

### 提示片段

技能可以通过声明提示片段文件向系统提示中注入内容。这适用于向 Agent 传授特定领域的知识、规则或行为，无需编写任何代码。

在 `manifest.json` 中添加 `prompts` 对象：

```json
{
  "name": "my-style-guide",
  "version": "1.0.0",
  "tools": [],
  "prompts": {
    "include": ["prompts/*.md"]
  }
}
```

**提示字段：**

| 字段 | 必填 | 说明 |
|-------|----------|-------------|
| `include` | 是 | 要包含的文件的 glob 模式数组 |

**路径解析：** glob 模式相对于技能目录解析。例如，`"prompts/*.md"` 匹配技能目录下 `prompts/` 子目录中的所有 `.md` 文件。

**行为：** 匹配的文件在加载时读取，其内容追加到系统提示中。文件按 glob 展开顺序处理。

**示例：技能目录布局**

```
~/.octos/skills/my-style-guide/
├── manifest.json
├── SKILL.md
└── prompts/
    ├── coding-rules.md
    └── review-checklist.md
```

对应的清单：

```json
{
  "name": "my-style-guide",
  "version": "1.0.0",
  "tools": [],
  "prompts": {
    "include": ["prompts/*.md"]
  }
}
```

当该技能激活时，`coding-rules.md` 和 `review-checklist.md` 都会被注入到系统提示中。

---

### 纯扩展技能

技能不需要提供任何可执行工具。如果 `manifest.json` 的 `tools` 数组为空（或完全省略），但声明了 `mcp_servers`、`hooks` 或 `prompts`，网关会加载扩展而不寻找二进制文件。这适用于：

- **纯提示注入技能** -- 一组 `.md` 文件，向 Agent 传授某个领域的知识
- **配置技能** -- 对所有工具调用执行策略的钩子
- **远程 MCP 技能** -- 运行在其他地方、通过 `url` 声明的 MCP 服务器

**示例：纯提示技能**

```json
{
  "name": "company-policy",
  "version": "1.0.0",
  "prompts": {
    "include": ["prompts/*.md"]
  }
}
```

没有 `tools`、没有二进制文件、没有 `mcp_servers`、没有 `hooks` -- 只有提示内容。

**示例：纯钩子技能**

```json
{
  "name": "audit-logger",
  "version": "1.0.0",
  "hooks": [
    {
      "event": "after_tool_call",
      "command": ["./hooks/log-to-siem.sh"],
      "timeout_ms": 5000
    }
  ]
}
```

没有工具 -- 技能只提供审计钩子。

**示例：组合扩展**

一个技能可以在常规工具之外同时声明全部三种扩展：

```json
{
  "name": "advanced-skill",
  "version": "1.0.0",
  "tools": [
    { "name": "analyze", "description": "Run analysis", "input_schema": { "type": "object" } }
  ],
  "mcp_servers": [
    { "command": "node", "args": ["mcp/server.js"], "env": ["API_KEY"] }
  ],
  "hooks": [
    { "event": "after_tool_call", "command": ["./hooks/audit.sh"], "tool_filter": ["analyze"] }
  ],
  "prompts": {
    "include": ["prompts/*.md"]
  }
}
```

---

### 完整清单字段参考

`manifest.json` 顶层字段的完整集合，包含扩展：

| 字段 | 必填 | 默认值 | 说明 |
|-------|----------|---------|-------------|
| `name` | 是 | -- | 技能标识符 |
| `version` | 是 | -- | 语义化版本 |
| `author` | 否 | -- | 作者名称 |
| `description` | 否 | -- | 可读的描述 |
| `timeout_secs` | 否 | 30 | 每次工具调用的最大执行时间（1-600） |
| `requires_network` | 否 | false | 信息性标志 |
| `sha256` | 否 | -- | 二进制完整性校验（十六进制哈希） |
| `tools` | 否 | `[]` | 工具定义数组 |
| `mcp_servers` | 否 | `[]` | MCP 服务器声明数组 |
| `hooks` | 否 | `[]` | 生命周期钩子定义数组 |
| `prompts` | 否 | -- | 提示片段配置对象 |
| `binaries` | 否 | `{}` | 按 `{os}-{arch}` 索引的预编译二进制文件 |

---

## 进阶主题

### 单个技能中的多个工具

一个技能二进制文件可以实现多个工具。工具名称通过 `argv[1]` 传递：

```rust
match tool_name {
    "get_weather" => handle_get_weather(&buf),
    "get_forecast" => handle_get_forecast(&buf),
    _ => fail(&format!("Unknown tool '{tool_name}'")),
}
```

每个工具必须在 `manifest.json` 中声明：

```json
{
  "tools": [
    { "name": "get_weather", "description": "...", "input_schema": { ... } },
    { "name": "get_forecast", "description": "...", "input_schema": { ... } }
  ]
}
```

### 环境变量

技能继承网关的环境（减去被屏蔽的变量）。使用 API 密钥的方式：

```rust
let api_key = std::env::var("MY_API_KEY")
    .unwrap_or_else(|_| fail("MY_API_KEY not set"));
```

在 SKILL.md frontmatter 中声明依赖，这样在环境变量缺失时技能会被标记为不可用：

```yaml
---
requires_env: MY_API_KEY
---
```

### 超时配置

在 `manifest.json` 中设置合理的超时：

| 技能类型 | 推荐超时 |
|------------|-------------------|
| 本地计算 | 5 秒 |
| 单次 API 调用 | 15 秒 |
| 多步 API 调用 | 30-60 秒 |
| 长时间研究 | 300-600 秒 |

### 安全

**二进制完整性：**

- **拒绝符号链接：** 插件二进制文件必须是常规文件。加载时会拒绝符号链接，以防范链接替换攻击。加载器使用 `symlink_metadata()`（而非 `metadata()`）来检测。
- **SHA-256 校验：** 如果 `manifest.json` 中存在 `sha256`，加载器会计算二进制文件的哈希值，不匹配则拒绝。已校验的字节写入单独的文件供网关执行，消除 TOCTOU（检查时间/使用时间）漏洞。
- **大小限制：** 插件可执行文件不得超过 100 MB。超大的二进制文件在读取前即被拒绝。

**环境清理：**

网关在启动技能进程前自动剥离以下环境变量：

- `LD_PRELOAD`、`DYLD_INSERT_LIBRARIES`、`DYLD_LIBRARY_PATH`
- `NODE_OPTIONS`、`PYTHONPATH`、`PERL5LIB`
- `RUSTFLAGS`、`RUST_LOG`
- 以及 10 余个其他变量（见 `sandbox.rs` 中的 `BLOCKED_ENV_VARS`）

**技能开发者的最佳实践：**

- 验证所有输入（永远不要信任 `city`、`path` 等）
- 为 HTTP 请求设置超时
- 避免 shell 注入（不要将用户输入传递给 shell 命令）
- 在发布构建中设置 `manifest.json` 中的 `sha256` 以启用完整性校验

### 平台技能 vs 应用技能

| | 应用技能 | 平台技能 |
|---|---|---|
| **位置** | `crates/app-skills/` | `crates/platform-skills/` |
| **数组** | `BUNDLED_APP_SKILLS` | `PLATFORM_SKILLS` |
| **引导** | 每次网关启动 | 仅管理员机器人 |
| **作用域** | 按网关 | 所有网关共享 |
| **使用场景** | 始终可用、自包含 | 需要外部服务 |

### 无需完整重建即可更新技能

技能可以独立重建和部署：

```bash
# 只构建该技能
cargo build --release -p weather

# 复制到远程服务器
scp target/release/weather remote:~/.octos/skills/weather/main

# 无需重启网关 — 下次工具调用时使用新二进制文件
```

注意：如果修改了 `SKILL.md` 或 `manifest.json`，则需要重新构建 `octos` 二进制文件（因为它们通过 `include_str!` 嵌入）。

---

## 安装与分发

### 技能类型

| 类型 | 位置 | 安装方式 | 二进制文件 | 使用场景 |
|------|----------|---------------|--------|----------|
| **内置** | `crates/app-skills/` | 编译进 `octos` 二进制 | 嵌入式 | 随每个版本发布的核心技能 |
| **外部** | GitHub 仓库 | `octos skills install user/repo` | 下载或构建 | 社区/自定义技能 |
| **配置文件本地** | `<profile-data>/skills/` | 按配置文件安装 | 自包含 | 租户隔离技能 |

### 按配置文件的技能管理

技能按配置文件安装以确保租户隔离。每个配置文件有独立的技能目录：

```
~/.octos/profiles/alice/data/
  skills/
    mofa-comic/
      main              ← 二进制文件（自包含，不在 ~/.cargo/bin）
      SKILL.md
      manifest.json
      styles/*.toml     ← 打包的资源文件
    mofa-slides/
      main
      SKILL.md
      manifest.json
      styles/*.toml
```

**重要：** 技能二进制文件以 `main` 的形式保留在其技能目录中。它们**不会**被复制到 `~/.cargo/bin/` 或任何全局位置。插件加载器在 `<skill-dir>/main` 处找到它们。

### 安装/卸载/列表命令

所有操作界面都支持按配置文件操作：

```bash
# CLI（--profile 标志放在子命令之前）
octos skills --profile alice install mofa-org/mofa-skills/mofa-comic
octos skills --profile alice list
octos skills --profile alice remove mofa-comic

# 聊天中（自动使用当前配置文件）
/skills install mofa-org/mofa-skills/mofa-comic
/skills list
/skills remove mofa-comic

# Admin API
POST /api/admin/profiles/alice/skills     {"repo": "mofa-org/mofa-skills/mofa-comic"}
GET  /api/admin/profiles/alice/skills
DELETE /api/admin/profiles/alice/skills/mofa-comic

# Agent 工具（自动使用当前配置文件）
manage_skills(action="install", repo="mofa-org/mofa-skills/mofa-comic")
manage_skills(action="list")
manage_skills(action="remove", name="mofa-comic")
manage_skills(action="search", query="comic")
```

### 技能加载优先级

网关从多个目录加载技能。名称冲突时先匹配者优先：

1. `<profile-data>/skills/` — 按配置文件（最高优先级）
2. `<project-dir>/skills/` — 项目本地
3. `<project-dir>/bundled-skills/` — 内置应用技能
4. `~/.octos/skills/` — 全局（最低优先级）

### 发布到注册表

外部技能可通过 [octos-hub](https://github.com/octos-org/octos-hub) 注册表被发现。

1. 将你的技能仓库推送到 GitHub
2. 通过 PR 向 `registry.json` 添加条目：

```json
{
  "name": "my-skills",
  "description": "What your skills do",
  "repo": "your-user/your-repo",
  "skills": ["skill-a", "skill-b"],
  "requires": ["git", "cargo"],
  "tags": ["keyword1", "keyword2"]
}
```

3. 用户即可查找并安装你的技能：

```bash
octos skills search keyword1
octos skills --profile alice install your-user/your-repo/skill-a
```

### 预编译二进制分发

为了加快安装速度（跳过编译），在 `manifest.json` 中添加 `binaries` 段：

```json
{
  "name": "my-skill",
  "version": "1.0.0",
  "binaries": {
    "darwin-aarch64": {
      "url": "https://github.com/you/repo/releases/download/v1.0.0/skill-darwin-aarch64.tar.gz",
      "sha256": "abc123..."
    },
    "darwin-x86_64": {
      "url": "https://github.com/you/repo/releases/download/v1.0.0/skill-darwin-x86_64.tar.gz",
      "sha256": "def456..."
    },
    "linux-x86_64": {
      "url": "https://github.com/you/repo/releases/download/v1.0.0/skill-linux-x86_64.tar.gz",
      "sha256": "789ghi..."
    }
  },
  "tools": [ ... ]
}
```

安装器下载匹配的二进制文件，验证 SHA-256，然后解压到 `<skill-dir>/main`。如果没有可用的预编译二进制文件，则回退到 `cargo build --release`。

### 技能的环境变量

网关自动向插件进程注入 API 密钥：

- 主提供商的 API 密钥（例如 `DASHSCOPE_API_KEY`）
- 备选提供商的密钥（例如 `GEMINI_API_KEY`、`OPENAI_API_KEY`）
- 非标准端点的 Base URL
- `OCTOS_DATA_DIR` 和 `OCTOS_WORK_DIR`

密钥在网关启动时从 macOS 钥匙串解析。技能二进制文件以环境变量的形式接收它们 -- 无需手动 export。

### 打包资源（样式、配置）

包含资源文件（样式、模板、配置）的技能应将它们打包在技能目录中：

```
my-skill/
  main
  SKILL.md
  manifest.json
  styles/
    default.toml
    manga.toml
  templates/
    report.html
```

二进制文件应相对于自身可执行文件的位置解析资源：

```rust
let exe = std::env::current_exe()?;
let skill_dir = exe.parent().unwrap();
let styles_dir = skill_dir.join("styles");
```

**不要**在工作目录（cwd）中查找资源 -- cwd 指向配置文件的数据目录，而非技能目录。

---

## 检查清单

### 工具技能（二进制文件 + 工具）

- [ ] 创建 `crates/app-skills/<name>/`，包含 Cargo.toml、manifest.json、SKILL.md、src/main.rs
- [ ] Cargo.toml 中的 `[[bin]] name` 与 bundled_app_skills.rs 中的 `binary_name` 匹配
- [ ] manifest.json 对所有工具输入有有效的 JSON Schema
- [ ] SKILL.md 有包含触发关键词的 frontmatter
- [ ] 二进制文件读取 `argv[1]` 获取工具名称，从 stdin 读取 JSON 输入
- [ ] 二进制文件向 stdout 输出 `{"output": "...", "success": true/false}`
- [ ] 错误情况返回 `success: false` 并附带清晰的消息
- [ ] 添加到工作区 `Cargo.toml` 的 members
- [ ] 添加到 `bundled_app_skills.rs` 中的 `BUNDLED_APP_SKILLS`
- [ ] `cargo build --workspace` 成功
- [ ] 独立测试：`echo '{"param": "value"}' | ./target/debug/my_skill my_tool`
- [ ] 网关测试：技能出现在 `~/.octos/skills/` 中且 Agent 可以使用

### 扩展（MCP 服务器、钩子、提示片段）

- [ ] `mcp_servers`：设置了 `command` 或 `url`；`env` 仅列出变量名称而非值
- [ ] `mcp_servers`：相对命令路径（`./bin/server`）在技能目录中存在
- [ ] `hooks`：`event` 是 `before_tool_call`、`after_tool_call`、`before_llm_call`、`after_llm_call` 之一
- [ ] `hooks`：`command` 是 argv 数组（非 shell 字符串）；`command[0]` 的相对路径能正确解析
- [ ] `hooks`：当钩子只应用于特定工具时设置了 `tool_filter`
- [ ] `prompts`：`include` 中的 glob 模式匹配技能目录中预期的 `.md` 文件
- [ ] 纯扩展技能：`tools` 数组为空或省略；不需要二进制文件
- [ ] 网关测试：扩展出现在加载器日志中（`loaded skill extras`）
