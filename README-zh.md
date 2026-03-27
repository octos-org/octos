# Octos

> 像章鱼一样——9个大脑（1个中央 + 8个在每条手臂中），每条手臂独立思考，但共享一个大脑。

**开放认知任务编排系统**（Open Cognitive Tasks Orchestration System）—— 一个 Rust 原生、API 优先的 Agentic 操作系统。

31MB 静态二进制。91 个 REST 端点。14 个 LLM 提供者。14 个消息频道。多租户。零依赖。

## 为什么选择 Octos

大多数 Agentic 系统是单租户聊天助手——一个用户、一个模型、一次一个对话。Octos 不同：

- **API 优先的 Agentic OS**：91 个 REST 端点（聊天、会话、管理、配置、技能、指标、Webhook）。任何前端——Web、移动端、CLI、CI/CD——都可以基于其构建。不锁定在聊天窗口中。
- **原生多租户设计**：一个 31MB 二进制在 16GB 机器上服务 200+ 配置。每个配置作为独立 OS 进程运行，拥有隔离的内存、会话和数据。Family Plan 子账户体系让父账户与子账户共享配置。
- **多 LLM DOT 流水线**：将工作流定义为 DOT 图。每个流水线节点可以使用不同的 LLM——搜索用便宜模型，综合用强力模型。动态并行扇出在运行时生成 N 个并发 Worker。
- **3 层提供者故障转移**：RetryProvider → ProviderChain → AdaptiveRouter。Hedge 模式同时竞速 2 个提供者。Lane 模式按延迟/错误率/质量/成本评分选择。熔断器自动禁用降级的提供者。
- **LRU 工具延迟加载**：15 个活跃工具确保快速 LLM 推理，34+ 个按需可用。LLM 调用 `activate_tools` 在对话中途加载专用工具。空闲工具自动淘汰。其他 Agentic 框架没有这个能力。
- **5 种队列模式（逐会话）**：Followup（先进先出）、Collect（合并批处理）、Steer（只保留最新）、Interrupt（取消当前）、Speculative（慢响应时并发执行）。用户通过 `/queue` 实时控制并发行为。
- **任意频道中的会话控制**：`/new`、`/s <名称>`、`/sessions`、`/back`——在 Telegram、Discord、Slack、WhatsApp 中都可使用。后台任务缓存最多 50 条消息；切回时自动刷出。
- **3 层记忆**：长期（MEMORY.md + 实体库含摘要，自动注入系统提示）、情景（任务结果存储在 redb，按工作目录向量检索）、会话（JSONL + 40+ 消息时 LLM 压缩）。
- **原生办公套件**：通过纯 Rust（zip + quick-xml）操作 PPTX/DOCX/XLSX。基本操作无需 LibreOffice 依赖。
- **沙箱隔离**：bwrap（Linux）+ sandbox-exec（macOS）+ Docker。全工作区 `deny(unsafe_code)`。67 项提示注入测试。macOS Keychain 存储密钥。常量时间令牌比较。

## 文档

**[用户指南（中文）](docs/user-guide-zh.md)** | **[User Guide (English)](docs/user-guide.md)**

完整指南覆盖：仪表盘设置、LLM 提供者、工具配置、配置管理、内置技能、平台技能（ASR/TTS）、自定义技能开发等。

---

## 功能特性

### 核心架构
- **31MB 静态二进制**：纯 Rust，零运行时依赖，支持 Linux x86_64、macOS ARM64 和 Docker Alpine 构建
- **91 个 REST 端点**：聊天、会话、管理、配置、技能、指标、Webhook、平台技能——在其上构建任何 UI
- **SSE 广播**：实时工具事件、LLM 令牌流式传输、进度更新推送给任意订阅者
- **Prometheus + JSON 指标**：CPU、内存、逐提供者延迟、P95——生产级可观测性

### LLM 与路由
- **14 个 LLM 提供者**：Anthropic、OpenAI、Gemini、OpenRouter、DeepSeek、Groq、Moonshot/Kimi、DashScope/Qwen、MiniMax、Zhipu/GLM、Z.AI、Nvidia NIM、Ollama、vLLM
- **3 层故障转移**：RetryProvider（指数退避）→ ProviderChain（多提供者 + 熔断器）→ AdaptiveRouter（指标驱动评分）
- **自适应路由模式**：Off（静态）、Hedge（竞速 2 个提供者，取赢家）、Lane（基于分数：延迟 35%、错误率 30%、质量 20%、成本 15%）
- **QoS 排名**：正交的质量开关——可与任何路由模式组合
- **自动升级**：检测到慢响应时自动启用 Hedge + Speculative 队列
- **提供者自动检测**：`--model gpt-4o` 自动选择 OpenAI
- **模型目录**：编程式发现，含能力、成本和别名查找

### 多租户与子账户
- **高密度多租户**：16GB Mac Mini 上 200+ 配置。每个配置是独立 OS 进程，拥有隔离的内存、会话和数据
- **Family Plan 子账户**：父配置与子配置共享设置。`octos account create/start/stop`
- **Web 仪表盘**：React SPA，逐用户配置管理、网关控制、实时日志流
- **邮件 OTP 认证**：飞书风格的邮件验证码登录
- **OAuth 登录**：PKCE 浏览器流、设备码流、粘贴令牌
- **舰队管理**：REST API 编程启停和监控所有配置

### 消息频道（14 个内置）
- **多频道网关**：CLI、Telegram、Discord、Slack、WhatsApp、飞书、邮件（IMAP/SMTP）、Twilio SMS、企微、企微机器人、微信、Matrix、QQ 机器人、API
- **会话控制**：`/new`、`/s <名称>`、`/sessions`、`/back`、`/delete`——在每个频道中可用
- **待处理消息**：会话不活跃时缓存最多 50 条消息，切换时自动刷出
- **完成通知**：后台任务跨会话通知完成状态
- **跨频道消息**：从任何频道发送消息到任何其他频道
- **消息合并**：频道感知的响应拆分（Telegram 4096、Discord 2000、Slack 限制）
- **媒体处理**：自动下载所有频道的照片、语音、音频、文档
- **视觉支持**：将图像发送到支持视觉的 LLM（Anthropic、OpenAI、Gemini、OpenRouter）

### Agent 并发（5 种队列模式）
- **Followup**：先进先出——逐条处理
- **Collect**（默认）：将所有排队消息合并为单条提示
- **Steer**：只保留最新消息，丢弃更早的
- **Interrupt**：取消当前 Agent 循环，立即处理新消息
- **Speculative**：慢响应时生成并发 Agent 任务——用户永不阻塞，两个结果都会送达
- 逐会话控制，通过 `/queue` 命令

### 工具与 LRU 延迟加载
- **13 个内置工具**：Shell、读/写/编辑文件、glob、grep、list_dir、网络搜索/抓取、git、浏览器、代码结构
- **12 个 Agent 级工具**：激活工具、生成子 Agent、深度搜索、综合研究、保存/召回记忆、管理技能、配置工具、消息、定时任务
- **8 个内置应用技能**：新闻、深度搜索、深度爬取、发送邮件、天气、账户管理、时钟、语音
- **LRU 工具延迟加载**：最多 15 个活跃，空闲 5 轮后自动淘汰。`activate_tools` 按需加载专用工具。无限目录，有限上下文窗口。
- **SafePolicy**：禁止 rm -rf、dd、mkfs、fork bomb。sudo、git push --force 需确认
- **并发执行**：所有工具调用通过 `join_all()` 并行运行
- **插件系统**：stdin/stdout JSON 协议——用任何语言编写插件

### 流水线编排
- **基于 DOT 的工作流**：将流水线定义为 Graphviz 有向图，含节点属性
- **逐节点模型选择**：每个节点可使用不同 LLM（搜索用便宜的，综合用强力的）
- **动态并行扇出**：`DynamicParallel` 处理器——LLM 规划器生成 N 个子任务，并发执行
- **处理器类型**：LLM、CodeGen、DynamicParallel、Parallel、Converge
- **质量门控**：`goal_gate` 在继续前检查输出质量
- **上下文保真度**：可配置的节点间上下文传递

### 记忆（3 层）
- **长期记忆**：MEMORY.md + 每日笔记（YYYY-MM-DD.md）+ 实体库（bank/entities/*.md 含摘要）。自动注入系统提示
- **情景记忆**：任务结果（成功/失败/阻塞）存储在 redb，含修改文件、关键决策。按工作目录向量检索
- **会话记忆**：JSONL 转录 + LRU 缓存（1000 会话）。40+ 消息时 LLM 压缩（保留最近 10 条，原子重写）
- **混合搜索**：HNSW 向量索引（16 连接，10K 容量）+ BM25 倒排索引。余弦 0.7 + BM25 0.3 混合
- **记忆工具**：`save_memory` + `recall_memory`（基于实体，先召回再合并再保存）

### 搜索
- **6 提供者故障转移**：Tavily → DuckDuckGo → Exa → Brave → You.com → Perplexity
- **深度搜索**：并行多查询，8 个并发 Worker
- **深度研究**：DOT 流水线——规划 → 搜索 → 分析 → 综合（多 LLM）
- **站点爬取**：全站爬取，支持深度和页面限制

### 安全
- **沙箱隔离**：bwrap（Linux）+ sandbox-exec（macOS）+ Docker（含资源限制）
- **`deny(unsafe_code)`**：全工作区，编译时强制
- **67 项提示注入测试**：prompt_guard + sanitize 模块
- **macOS Keychain**：通过 `security` CLI 安全存储密钥
- **常量时间比较**：`constant_time_eq` + `subtle` crate
- **工具策略**：允许/拒绝列表、通配符匹配、命名组、逐提供者过滤
- **SSRF 防护**：阻止私有 IP 范围
- **环境变量清洗**：阻止敏感环境变量

### 语音与办公
- **TTS**：Qwen3-TTS 语音技能（通过参考音频进行声音克隆）
- **ASR**：Qwen3-ASR（通过 ominix-api 平台技能）
- **办公工具**：原生 PPTX/DOCX/XLSX 操作（zip + quick-xml）——提取、打包、验证、添加幻灯片、接受修订
- **可安装技能**：mofa-cards、mofa-comic、mofa-infographic、mofa-slides（17 种风格，4K）通过 octos-hub

### 开发者体验
- **1,477 个测试**：单元测试（15 秒）+ 集成测试（5 分钟）
- **cargo fmt + clippy**：CI 中以 `-D warnings` 强制执行
- **类型化 LLM 错误**：结构化错误层级（速率限制、认证、上下文溢出）含可重试性分类
- **LLM 中间件**：可组合的拦截器（日志、成本追踪、缓存）
- **高级客户端**：`generate()`、`generate_object()`、`generate_typed<T>()`、`stream()` API
- **配置迁移**：版本化配置，自动迁移
- **配置热重载**：SHA-256 变更检测，实时系统提示更新
- **MCP 集成**：JSON-RPC stdio 传输，用于 Model Context Protocol 服务器
- **纯 Rust TLS**：无 OpenSSL 依赖（使用 rustls）

---

## 快速开始

```bash
# 初始化配置和工作区
octos init

# 设置 API 密钥（或使用 OAuth 登录）
export ANTHROPIC_API_KEY=your-key-here
# 或：octos auth login -p anthropic

# 交互式聊天
octos chat

# 单消息模式（非交互）
octos chat --message "给 lib.rs 添加一个 hello 函数"

# 查看系统状态
octos status

# 启动多频道网关
octos gateway

# 启动 Web 仪表盘 + REST API
octos serve
```

完整安装和配置说明请参阅 **[用户指南（中文）](docs/user-guide-zh.md)** 或 **[README (English)](README.md)**。

## 许可证

详见 [LICENSE](LICENSE) 文件。
