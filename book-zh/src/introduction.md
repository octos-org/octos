# 简介

> 🌐 **[English Documentation](../../)**

## Octos 是什么？

Octos 是一个开源 AI 智能体平台，能将任意大语言模型变成多渠道、多用户的智能助手。你只需部署一个 Rust 编译的二进制文件，配置好 LLM API 密钥和消息渠道（Telegram、Discord、Slack、WhatsApp、Email、微信等），Octos 会处理其余一切——对话路由、工具执行、记忆管理、模型故障切换，以及多租户隔离。

可以把它理解为 **AI 智能体的后端操作系统**。你无需为每个场景从零搭建聊天机器人，只需配置 Octos 的 Profile——每个 Profile 拥有独立的系统提示词、模型、工具和渠道——然后通过 Web 仪表板或 REST API 统一管理。一个小团队就能在一台机器上运行数百个专用 AI 智能体。

Octos 面向那些需求超越个人助手的用户：需要在 WhatsApp 和 Telegram 上部署 AI 客服的团队、希望在 REST API 之上构建 AI 产品的开发者、使用不同 LLM 编排多步骤研究流程的科研人员，或是共享一套 AI 系统并为每位家庭成员提供个性化配置的家庭用户。

## 运行模式

Octos 有两种主要运行模式：

- **对话模式** (`octos chat`)：交互式多轮对话，支持工具调用；也可通过 `--message` 发送单条消息后退出。
- **网关模式** (`octos gateway`)：常驻守护进程，同时服务多个消息渠道。

## 核心概念

| 术语 | 说明 |
|------|------|
| **Agent（智能体）** | 使用工具执行任务的 AI |
| **Tool（工具）** | 一项能力（Shell、文件操作、搜索、消息发送等） |
| **Provider（供应商）** | LLM API 服务（Anthropic、OpenAI 等） |
| **Channel（渠道）** | 消息平台（CLI、Telegram、Slack 等） |
| **Session（会话）** | 按渠道和聊天 ID 划分的对话历史 |
| **Sandbox（沙箱）** | 隔离的执行环境（bwrap、macOS sandbox-exec、Docker） |
| **Tool Policy（工具策略）** | 控制可用工具的允许/拒绝规则 |
| **Skill（技能）** | 可复用的指令模板（SKILL.md） |
| **Bootstrap（引导文件）** | 加载到系统提示词中的上下文文件（AGENTS.md、SOUL.md 等） |
