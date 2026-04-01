# Introduction

> 🌐 **[中文文档](../zh/)**

## What is Octos?

Octos is an open-source AI agent platform that turns any LLM into a multi-channel, multi-user intelligent assistant. You deploy a single Rust binary, connect your LLM API keys and messaging channels (Telegram, Discord, Slack, WhatsApp, Email, WeChat, and more), and Octos handles everything else -- conversation routing, tool execution, memory, provider failover, and multi-tenant isolation.

Think of it as the **backend operating system for AI agents**. Instead of building a chatbot from scratch for each use case, you configure Octos profiles -- each with their own system prompt, model, tools, and channels -- and manage them all through a web dashboard or REST API. A small team can run hundreds of specialized AI agents on a single machine.

Octos is built for people who need more than a personal assistant: teams deploying AI for customer support across WhatsApp and Telegram, developers building AI-powered products on top of a REST API, researchers orchestrating multi-step research pipelines with different LLMs at each stage, or families sharing a single AI setup with per-person customization.

## Operating Modes

Octos operates in two primary modes:

- **Chat mode** (`octos chat`): Interactive multi-turn conversation with tools, or single-message execution via `--message`.
- **Gateway mode** (`octos gateway`): Persistent daemon serving multiple messaging channels simultaneously.

## Key Concepts

| Term | Description |
|------|-------------|
| **Agent** | AI that executes tasks using tools |
| **Tool** | A capability (shell, file ops, search, messaging) |
| **Provider** | LLM API service (Anthropic, OpenAI, etc.) |
| **Channel** | Messaging platform (CLI, Telegram, Slack, etc.) |
| **Session** | Conversation history per channel and chat ID |
| **Sandbox** | Isolated execution environment (bwrap, macOS sandbox-exec, Docker) |
| **Tool Policy** | Allow/deny rules controlling which tools are available |
| **Skill** | Reusable instruction template (SKILL.md) |
| **Bootstrap** | Context files loaded into system prompt (AGENTS.md, SOUL.md, etc.) |
