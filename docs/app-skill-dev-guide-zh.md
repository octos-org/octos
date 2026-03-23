# Octos 技能开发指南

[English](app-skill-dev-guide.md) | [中文](app-skill-dev-guide-zh.md)

本指南涵盖构建、注册和部署 octos 技能所需的全部内容。

---

## 架构概述

技能是一个**独立可执行二进制文件**，通过简单的 **stdin/stdout JSON 协议**与 octos 网关通信。网关为每次工具调用生成技能进程，通过 stdin 传递 JSON 参数，从 stdout 读取 JSON 结果。

```
用户消息 → LLM → tool_use("get_weather", {"city": "巴黎"})
                         ↓
              网关生成进程: <skills-dir>/weather/main get_weather
                         ↓
              Stdin:  {"city": "巴黎"}
              Stdout: {"output": "巴黎，法国\n晴朗\n...", "success": true}
                         ↓
              LLM 看到结果 → 生成自然语言回复
```

---

## 技能目录结构

```
my-skill/
├── SKILL.md            # 必需：文档 + 前置元数据
├── manifest.json       # 可选：工具定义（JSON Schema）+ 二进制分发
├── Cargo.toml          # 可选：Rust 源码（安装时自动编译）
├── package.json        # 可选：Node.js（安装时自动 npm install）
├── styles/             # 可选：样式/模板等资源文件
├── references/         # 可选：参考文档
└── src/
    └── main.rs         # 可选：工具源代码
```

安装后，技能位于配置文件的数据目录中：

```
~/.octos/profiles/alice/data/skills/my-skill/
├── main                # 可执行二进制（自包含，不放在 ~/.cargo/bin）
├── manifest.json       # 工具定义
├── SKILL.md            # 文档
└── styles/             # 资源文件
```

---

## 分步创建新技能

### 1. SKILL.md

每个技能需要一个带 YAML 前置元数据的 `SKILL.md`：

```markdown
---
name: my-skill
description: 在技能列表中显示的简短描述。触发词：关键词1, keyword2, 关键词3
version: 1.0.0
author: your-name
always: false
requires_bins: docker,ffmpeg
requires_env: API_KEY
---

# 我的技能

给智能体的详细指令。像给同事做简报一样来写：
- 这个技能做什么
- 什么时候使用
- 分步骤的使用方式
- 带预期输出的示例
```

#### 前置元数据字段

| 字段 | 必填 | 默认值 | 说明 |
|------|------|--------|------|
| `name` | 是 | — | 小写标识符，用连字符（如 `deep-search`） |
| `description` | 是 | — | 一行描述，显示在 `octos skills list` 中。包含触发关键词 |
| `version` | 否 | — | 语义化版本号 |
| `author` | 否 | — | 作者名或组织 |
| `always` | 否 | `false` | `true` = 始终包含在系统提示中。谨慎使用 |
| `requires_bins` | 否 | — | 逗号分隔的 PATH 中必须存在的可执行文件 |
| `requires_env` | 否 | — | 逗号分隔的必须设置的环境变量 |

### 2. manifest.json

定义 LLM 可以调用的工具。使用 JSON Schema 进行输入验证。

```json
{
  "name": "my-skill",
  "version": "1.0.0",
  "timeout_secs": 15,
  "tools": [
    {
      "name": "my_tool",
      "description": "工具的功能描述（展示给 LLM）",
      "input_schema": {
        "type": "object",
        "properties": {
          "param1": { "type": "string", "description": "参数含义" },
          "param2": { "type": "integer", "description": "可选数字参数", "default": 10 }
        },
        "required": ["param1"]
      }
    }
  ]
}
```

#### 清单字段

| 字段 | 必填 | 默认值 | 说明 |
|------|------|--------|------|
| `name` | 是 | — | 技能标识符 |
| `version` | 是 | — | 语义化版本 |
| `timeout_secs` | 否 | 30 | 每次工具调用的最大执行时间（1-600） |
| `tools` | 否 | `[]` | 工具定义数组 |
| `binaries` | 否 | `{}` | 预编译二进制，按 `{os}-{arch}` 分类 |
| `mcp_servers` | 否 | `[]` | MCP 服务器声明 |
| `hooks` | 否 | `[]` | 生命周期钩子 |
| `prompts` | 否 | — | 提示片段配置 |

### 3. src/main.rs（Rust 工具）

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
        fail(&format!("读取 stdin 失败: {e}"));
    }

    match tool_name {
        "my_tool" => handle_my_tool(&buf),
        _ => fail(&format!("未知工具 '{tool_name}'")),
    }
}

fn fail(msg: &str) -> ! {
    println!("{}", json!({"output": msg, "success": false}));
    std::process::exit(1);
}

fn handle_my_tool(input_json: &str) {
    let input: MyToolInput = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(e) => fail(&format!("无效输入: {e}")),
    };

    let result = format!("处理 {} 参数2={}", input.param1, input.param2);
    println!("{}", json!({"output": result, "success": true}));
}
```

### Node.js 工具

```javascript
// index.js
const input = JSON.parse(require('fs').readFileSync('/dev/stdin', 'utf8'));
const result = `查询结果: ${input.query}`;
console.log(JSON.stringify({ output: result, success: true }));
```

### 工具 I/O 协议

| 规则 | 说明 |
|------|------|
| **argv[1]** | 工具名（如 `get_weather`） |
| **stdin** | 匹配工具 `input_schema` 的 JSON 对象 |
| **stdout** | `{"output": "结果文本", "success": true/false}` |
| **退出码** | 0 = 成功，非零 = 失败 |
| **stderr** | 网关忽略（用于调试日志） |

---

## 安装与分发

### 技能类型

| 类型 | 位置 | 安装方式 | 用途 |
|------|------|----------|------|
| **内置** | `crates/app-skills/` | 编译进 `octos` 二进制 | 每次发布附带的核心技能 |
| **外部** | GitHub 仓库 | `octos skills install user/repo` | 社区/自定义技能 |
| **配置文件级** | `<profile-data>/skills/` | 按配置文件安装 | 租户隔离的技能 |

### 按配置文件管理技能

技能按配置文件安装，确保租户隔离。每个配置文件有自己的技能目录：

```
~/.octos/profiles/alice/data/
  skills/
    mofa-comic/
      main              ← 二进制（自包含，不在 ~/.cargo/bin）
      SKILL.md
      manifest.json
      styles/*.toml     ← 打包的资源
```

**重要：** 技能二进制保存在技能目录中的 `main` 文件。不会复制到 `~/.cargo/bin/` 或任何全局位置。

### 安装/卸载/列表命令

所有操作界面均支持按配置文件操作：

```bash
# CLI（--profile 标志放在子命令之前）
octos skills --profile alice install mofa-org/mofa-skills/mofa-comic
octos skills --profile alice list
octos skills --profile alice remove mofa-comic

# 聊天中（自动使用当前配置文件）
/skills install mofa-org/mofa-skills/mofa-comic
/skills list
/skills remove mofa-comic

# 管理 API
POST   /api/admin/profiles/alice/skills     {"repo": "mofa-org/mofa-skills/mofa-comic"}
GET    /api/admin/profiles/alice/skills
DELETE /api/admin/profiles/alice/skills/mofa-comic

# 智能体工具（自动使用当前配置文件）
manage_skills(action="install", repo="mofa-org/mofa-skills/mofa-comic")
manage_skills(action="list")
manage_skills(action="remove", name="mofa-comic")
manage_skills(action="search", query="comic")
```

### 技能加载优先级

网关从多个目录加载技能。同名冲突时先到先得：

1. `<profile-data>/skills/` — 配置文件级（最高优先级）
2. `<project-dir>/skills/` — 项目本地
3. `<project-dir>/bundled-skills/` — 内置应用技能
4. `~/.octos/skills/` — 全局（最低优先级）

### 发布到注册中心

外部技能可通过 [octos-hub](https://github.com/octos-org/octos-hub) 注册中心被发现。

1. 将技能仓库推送到 GitHub
2. 通过 PR 向 `registry.json` 添加条目：

```json
{
  "name": "my-skills",
  "description": "你的技能包描述",
  "repo": "your-user/your-repo",
  "skills": ["skill-a", "skill-b"],
  "requires": ["git", "cargo"],
  "tags": ["关键词1", "关键词2"]
}
```

3. 用户即可搜索和安装：

```bash
octos skills search 关键词1
octos skills --profile alice install your-user/your-repo/skill-a
```

### 预编译二进制分发

为加速安装（跳过编译），在 `manifest.json` 中添加 `binaries` 部分：

```json
{
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
  }
}
```

**平台标识**：`darwin-aarch64`（Apple Silicon）、`darwin-x86_64`（Intel Mac）、`linux-x86_64`、`linux-aarch64`。

安装器下载匹配的二进制文件，验证 SHA-256 后解压到 `<skill-dir>/main`。如果没有预编译版本，则回退到 `cargo build --release`。

### 技能的环境变量

网关自动向插件进程注入 API 密钥：

- 主提供商的 API 密钥（如 `DASHSCOPE_API_KEY`）
- 备用提供商密钥（如 `GEMINI_API_KEY`、`OPENAI_API_KEY`）
- 非标准端点的 Base URL
- `OCTOS_DATA_DIR` 和 `OCTOS_WORK_DIR`

密钥在网关启动时从 macOS 钥匙串解析。技能二进制通过环境变量接收 — 无需手动导出。

### 打包资源（样式、配置）

包含资源文件（样式、模板、配置）的技能应将其打包在技能目录中：

```
my-skill/
  main
  SKILL.md
  manifest.json
  styles/
    default.toml
    manga.toml
```

二进制应相对于自身可执行文件位置解析资源：

```rust
let exe = std::env::current_exe()?;
let skill_dir = exe.parent().unwrap();
let styles_dir = skill_dir.join("styles");
```

**不要**在工作目录（cwd）中查找资源 — cwd 指向配置文件的数据目录，而非技能目录。

---

## 清单扩展：MCP 服务器、钩子和提示片段

技能在 `manifest.json` 中不仅可以声明工具，还支持三个额外扩展点。

### MCP 服务器

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

| 字段 | 必填 | 说明 |
|------|------|------|
| `command` | 否* | 生成 MCP 服务器进程的命令 |
| `args` | 否 | 传递给命令的参数 |
| `env` | 否 | 要转发的环境变量**名称**列表（非值） |
| `url` | 否* | HTTP 传输：远程 MCP 服务器 URL |

\* `command` 和 `url` 二选一。

### 生命周期钩子

```json
{
  "hooks": [
    {
      "event": "before_tool_call",
      "command": ["./hooks/policy-check.sh"],
      "timeout_ms": 3000,
      "tool_filter": ["shell"]
    }
  ]
}
```

| 事件 | 可拒绝？ | 触发时机 |
|------|----------|----------|
| `before_tool_call` | 是 | 工具执行前。退出码 1 = 拒绝 |
| `after_tool_call` | 否 | 工具执行后 |
| `before_llm_call` | 是 | LLM 请求发送前 |
| `after_llm_call` | 否 | LLM 响应接收后 |

### 提示片段

```json
{
  "prompts": {
    "include": ["prompts/*.md"]
  }
}
```

匹配的文件在加载时读取并追加到系统提示中。

### 纯扩展技能

技能不需要提供工具可执行文件。如果 `manifest.json` 的 `tools` 数组为空但声明了 `mcp_servers`、`hooks` 或 `prompts`，网关加载扩展而不查找二进制。适用于：

- **纯提示注入技能** — 教授智能体领域知识的 `.md` 文件集合
- **策略技能** — 对所有工具调用强制执行策略的钩子
- **远程 MCP 技能** — 通过 `url` 声明的远程 MCP 服务器

---

## 多技能仓库

单个仓库可以包含多个技能作为顶层目录：

```
my-skills/
  skill-a/
    SKILL.md
  skill-b/
    SKILL.md
    manifest.json
    src/main.rs
```

```bash
octos skills install you/my-skills          # 安装全部
octos skills install you/my-skills/skill-b  # 仅安装 skill-b
```

---

## 安全

### 二进制完整性

- **符号链接拒绝：** 插件二进制必须是常规文件，符号链接在加载时被拒绝
- **SHA-256 验证：** 如果 `manifest.json` 中有 `sha256`，加载器计算哈希并在不匹配时拒绝
- **大小限制：** 插件可执行文件必须小于 100 MB

### 环境清理

网关在生成技能进程前自动清除以下环境变量：

- `LD_PRELOAD`、`DYLD_INSERT_LIBRARIES`、`DYLD_LIBRARY_PATH`
- `NODE_OPTIONS`、`PYTHONPATH`、`PERL5LIB`
- `RUSTFLAGS`、`RUST_LOG`
- 以及 10+ 其他（参见 `sandbox.rs` 中的 `BLOCKED_ENV_VARS`）

### 技能作者最佳实践

- 验证所有输入（不要信任 `city`、`path` 等）
- 对 HTTP 请求设置超时
- 避免 shell 注入（不要将用户输入传递给 shell 命令）
- 在 `manifest.json` 中设置 `sha256` 以启用完整性验证

---

## 构建与测试

```bash
# 构建技能
cargo build -p my-skill

# 独立测试
echo '{"param1": "hello"}' | ./target/debug/my_skill my_tool

# 预期输出：
# {"output":"处理 hello 参数2=10","success":true}

# 构建发布版本
cargo build --release -p my-skill
```

---

## 检查清单

### 工具技能（二进制 + 工具）

- [ ] 创建技能目录，包含 SKILL.md、manifest.json
- [ ] manifest.json 中所有工具输入有有效的 JSON Schema
- [ ] SKILL.md 有包含触发关键词的前置元数据
- [ ] 二进制读取 `argv[1]` 获取工具名，stdin 获取 JSON 输入
- [ ] 二进制向 stdout 写入 `{"output": "...", "success": true/false}`
- [ ] 错误情况返回 `success: false` 及清晰的错误消息
- [ ] 独立测试：`echo '{"param": "value"}' | ./main my_tool`
- [ ] 网关测试：技能出现在技能列表中且智能体可以使用

### 扩展（MCP 服务器、钩子、提示片段）

- [ ] `mcp_servers`：设置了 `command` 或 `url`
- [ ] `hooks`：`event` 是支持的事件之一
- [ ] `hooks`：`command` 是 argv 数组（非 shell 字符串）
- [ ] `prompts`：`include` 中的 glob 模式匹配预期的文件
- [ ] 纯扩展技能：`tools` 数组为空或省略
