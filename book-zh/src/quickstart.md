# 快速上手

本指南带你快速完成 Octos 的基本配置和运行。

## 1. 初始化工作区

进入你的项目目录，初始化 Octos：

```bash
cd your-project
octos init
```

该命令会创建 `.octos/` 目录，包含默认配置、引导文件（AGENTS.md、SOUL.md、USER.md），以及记忆、会话和技能的子目录。

## 2. 设置 API 密钥

至少导出一个 LLM 供应商的密钥：

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
```

将此行添加到 `~/.bashrc` 或 `~/.zshrc` 中以持久保存。你也可以使用 `octos auth login --provider openai` 进行 OAuth 登录。

## 3. 检查配置

验证所有配置是否正确：

```bash
octos status
```

该命令会显示配置文件位置、当前使用的供应商和模型、API 密钥状态，以及引导文件的可用情况。

## 4. 开始对话

启动交互式多轮对话：

```bash
octos chat
```

或发送单条消息后退出：

```bash
octos chat --message "Add a hello function to lib.rs"
```

## 5. 运行网关

以常驻守护进程的方式服务多个消息渠道：

```bash
octos gateway
```

此命令要求配置文件中包含 `gateway` 部分，且至少配置了一个渠道。详见[配置](configuration.md)章节。

## 6. 启动 Web 界面

如果编译时启用了 `api` 特性，可以启动 Web 仪表板：

```bash
octos serve
```

然后在浏览器中打开 `http://localhost:8080`。
