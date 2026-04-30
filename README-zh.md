# Octos 🐙

> 像章鱼一样——9 个大脑（1 个中央 + 8 个分布在手臂中，每条手臂一个）。每条手臂独立思考，但共享一个大脑。

**开放认知任务编排系统** — 一个 Rust 原生、API 优先的 Agentic 操作系统。

31MB 静态二进制。约 140 个 REST 端点。15 个 LLM 提供者。14 个消息频道。多租户。零外部运行时依赖。

## Octos 是什么？

Octos 是一个开源 AI Agent 平台，能将任何 LLM 变成多频道、多用户的智能助手。你只需部署一个 Rust 二进制文件，连接 LLM API 密钥和消息频道（Telegram、Discord、Slack、WhatsApp、Matrix、邮件、微信、企业微信、飞书、Twilio 短信、QQ 等），Octos 会处理其余一切——对话路由、工具执行、记忆、提供者故障转移和多租户隔离。

可以把它理解为 **AI Agent 的后端操作系统**。无需为每个场景从零构建聊天机器人，你只需配置 Octos 配置文件——每个配置拥有独立的系统提示、模型、工具和频道——然后通过 Web 仪表盘或 REST API 统一管理。一个小团队可以在一台机器上运行数百个专用 AI Agent。

## 为什么选择 Octos

- **API 优先的 Agentic OS**：约 140 个 REST 端点（chat、sessions、admin、profiles、skills、swarm、pipeline、metrics、webhooks、SSE）。任何前端——Web、移动端、CLI、CI/CD——都可以基于其构建。
- **原生多租户**：16GB 机器上 200+ 配置。每个配置是独立 OS 进程。Family Plan 子账户体系。
- **多 LLM DOT 流水线**：DOT 图定义工作流。逐节点模型选择。动态并行扇出，带有限流以保证 Fleet 稳定性。
- **Swarm 调度器**：将契约扇出到 N 个子 Agent，聚合产物，通过校验器审核，汇总成本——已接入 `/api/swarm/dispatch`。
- **3 层提供者故障转移**：RetryProvider → ProviderChain → AdaptiveRouter。Hedge 竞速、Lane 评分、熔断器。
- **LRU 工具延迟加载**：约 15 个活跃工具，约 50 个按需可用。空闲工具自动淘汰。`spawn_only` 工具自动转后台执行。
- **5 种队列模式**：Followup、Collect、Steer、Interrupt、Speculative——通过 `/queue` 控制 Agent 并发。
- **任意频道会话控制**：`/new`、`/s <名称>`、`/sessions`、`/back`——在 Telegram、Discord、Slack、WhatsApp、Matrix、飞书中可用。
- **Sticky thread_id + committed_seq**：每个 SSE 事件都绑定到 thread；按提交序号确定性回放（M8.10）。
- **3 层记忆**：长期（实体库，自动注入）、情景（任务结果在 redb）、会话（JSONL + LLM 三层压缩）。
- **原生办公套件**：纯 Rust 操作 PPTX/DOCX/XLSX。
- **沙箱隔离**：bwrap + sandbox-exec + Docker + Windows AppContainer。全工作区 `deny(unsafe_code)`。67 项注入测试。

## 快速开始

```bash
# 默认 features 与 scripts/milestone-ci.sh 保持一致。
# 不带 features 安装会得到一个缺少 `octos serve` 与各频道适配器的二进制。
cargo install --path crates/octos-cli \
    --features "api,telegram,discord,whatsapp,feishu,twilio,wecom,wecom-bot"
octos init
export ANTHROPIC_API_KEY=your-key-here
octos chat
```

## 文档

📖 **[完整文档](https://octos-org.github.io/octos/zh/)** — 安装、配置、频道、提供者、记忆、技能、高级功能等。

**English:** [English README](README.md) | [Documentation](https://octos-org.github.io/octos/)

## 许可证

详见 [LICENSE](LICENSE) 文件。
