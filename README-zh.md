# Octos 🐙

> 像章鱼一样——9个大脑（1个中央 + 8个在每条手臂中），每条手臂独立思考，但共享一个大脑。

**开放认知任务编排系统** — 一个 Rust 原生、API 优先的 Agentic 操作系统。

31MB 静态二进制。91 个 REST 端点。14 个 LLM 提供者。14 个消息频道。多租户。零依赖。

## Octos 是什么？

Octos 是一个开源 AI Agent 平台，能将任何 LLM 变成多频道、多用户的智能助手。你只需部署一个 Rust 二进制文件，连接 LLM API 密钥和消息频道（Telegram、Discord、Slack、WhatsApp、邮件、微信等），Octos 会处理其余一切——对话路由、工具执行、记忆、提供者故障转移和多租户隔离。

可以把它理解为 **AI Agent 的后端操作系统**。无需为每个场景从零构建聊天机器人，你只需配置 Octos 配置文件——每个配置拥有独立的系统提示、模型、工具和频道——然后通过 Web 仪表盘或 REST API 统一管理。一个小团队可以在一台机器上运行数百个专用 AI Agent。

## 为什么选择 Octos

- **API 优先的 Agentic OS**：91 个 REST 端点。任何前端——Web、移动端、CLI、CI/CD——都可以基于其构建。
- **原生多租户**：16GB 机器上 200+ 配置。每个配置是独立 OS 进程。Family Plan 子账户体系。
- **多 LLM DOT 流水线**：DOT 图定义工作流。逐节点模型选择。动态并行扇出。
- **3 层提供者故障转移**：RetryProvider → ProviderChain → AdaptiveRouter。Hedge 竞速、Lane 评分、熔断器。
- **LRU 工具延迟加载**：15 个活跃工具，34+ 按需可用。空闲工具自动淘汰。
- **5 种队列模式**：Followup、Collect、Steer、Interrupt、Speculative——通过 `/queue` 控制 Agent 并发。
- **任意频道会话控制**：`/new`、`/s <名称>`、`/sessions`、`/back`——在 Telegram、Discord、Slack、WhatsApp 中可用。
- **3 层记忆**：长期（实体库，自动注入）、情景（任务结果在 redb）、会话（JSONL + LLM 压缩）。
- **原生办公套件**：纯 Rust 操作 PPTX/DOCX/XLSX。
- **沙箱隔离**：bwrap + sandbox-exec + Docker。全工作区 `deny(unsafe_code)`。67 项注入测试。

## 快速开始

```bash
cargo install --path crates/octos-cli
octos init
export ANTHROPIC_API_KEY=your-key-here
octos chat
```

## 文档

📖 **[完整文档](https://octos-org.github.io/octos/zh/)** — 安装、配置、频道、提供者、记忆、技能、高级功能等。

**English:** [English README](README.md) | [Documentation](https://octos-org.github.io/octos/)

## 许可证

详见 [LICENSE](LICENSE) 文件。
