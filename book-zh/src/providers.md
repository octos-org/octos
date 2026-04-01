# LLM 服务商与路由

Octos 开箱即用地支持 14 家 LLM 服务商。每个服务商需要一个存储在环境变量中的 API 密钥（本地服务商如 Ollama 除外）。

## 支持的服务商

| 服务商 | 环境变量 | 默认模型 | API 格式 | 别名 |
|----------|-------------|---------------|------------|---------|
| `anthropic` | `ANTHROPIC_API_KEY` | claude-sonnet-4-20250514 | Native Anthropic | -- |
| `openai` | `OPENAI_API_KEY` | gpt-4o | Native OpenAI | -- |
| `gemini` | `GEMINI_API_KEY` | gemini-2.0-flash | Native Gemini | -- |
| `openrouter` | `OPENROUTER_API_KEY` | anthropic/claude-sonnet-4-20250514 | Native OpenRouter | -- |
| `deepseek` | `DEEPSEEK_API_KEY` | deepseek-chat | OpenAI 兼容 | -- |
| `groq` | `GROQ_API_KEY` | llama-3.3-70b-versatile | OpenAI 兼容 | -- |
| `moonshot` | `MOONSHOT_API_KEY` | kimi-k2.5 | OpenAI 兼容 | `kimi` |
| `dashscope` | `DASHSCOPE_API_KEY` | qwen-max | OpenAI 兼容 | `qwen` |
| `minimax` | `MINIMAX_API_KEY` | MiniMax-Text-01 | OpenAI 兼容 | -- |
| `zhipu` | `ZHIPU_API_KEY` | glm-4-plus | OpenAI 兼容 | `glm` |
| `zai` | `ZAI_API_KEY` | glm-5 | Anthropic 兼容 | `z.ai` |
| `nvidia` | `NVIDIA_API_KEY` | meta/llama-3.3-70b-instruct | OpenAI 兼容 | `nim` |
| `ollama` | *（无需）* | llama3.2 | OpenAI 兼容 | -- |
| `vllm` | `VLLM_API_KEY` | *（须指定）* | OpenAI 兼容 | -- |

## 配置方式

### 配置文件

在 `config.json` 中设置 `provider` 和 `model`：

```json
{
  "provider": "moonshot",
  "model": "kimi-2.5",
  "api_key_env": "KIMI_API_KEY"
}
```

`api_key_env` 字段可覆盖服务商默认的环境变量名。例如，Moonshot 默认使用 `MOONSHOT_API_KEY`，但你可以将其指向 `KIMI_API_KEY`。

### 命令行参数

```bash
octos chat --provider deepseek --model deepseek-chat
octos chat --model gpt-4o  # 根据模型名自动检测服务商
```

### 凭证存储

除了环境变量，你也可以通过 auth 命令行存储 API 密钥：

```bash
# OAuth PKCE (OpenAI)
octos auth login --provider openai

# Device code 流程 (OpenAI)
octos auth login --provider openai --device-code

# 粘贴令牌（其他所有服务商）
octos auth login --provider anthropic
# -> 提示: "Paste your API key:"

# 查看已存储的凭证
octos auth status

# 删除凭证
octos auth logout --provider openai
```

凭证存储在 `~/.octos/auth.json`（文件权限 0600）。解析 API 密钥时，凭证存储的优先级**高于**环境变量。

## 自动检测

省略 `--provider` 时，Octos 会根据模型名推断服务商：

| 模型名模式 | 检测到的服务商 |
|--------------|-------------------|
| `claude-*` | anthropic |
| `gpt-*`, `o1-*`, `o3-*`, `o4-*` | openai |
| `gemini-*` | gemini |
| `deepseek-*` | deepseek |
| `kimi-*`, `moonshot-*` | moonshot |
| `qwen-*` | dashscope |
| `glm-*` | zhipu |
| `llama-*` | groq |

```bash
octos chat --model gpt-4o           # -> openai
octos chat --model claude-sonnet-4-20250514  # -> anthropic
octos chat --model deepseek-chat    # -> deepseek
octos chat --model glm-4-plus       # -> zhipu
octos chat --model qwen-max         # -> dashscope
```

## 自定义端点

使用 `base_url` 指向自部署或代理端点：

```json
{
  "provider": "openai",
  "model": "gpt-4o",
  "base_url": "https://your-azure-endpoint.openai.azure.com/v1"
}
```

```json
{
  "provider": "ollama",
  "model": "llama3.2",
  "base_url": "http://localhost:11434/v1"
}
```

```json
{
  "provider": "vllm",
  "model": "meta-llama/Llama-3-70b",
  "base_url": "http://localhost:8000/v1"
}
```

### API 类型覆盖

`api_type` 字段可在服务商使用非标准协议时强制指定传输格式：

```json
{
  "provider": "zai",
  "model": "glm-5",
  "api_type": "anthropic"
}
```

- `"openai"` -- OpenAI Chat Completions 格式（大多数服务商的默认值）
- `"anthropic"` -- Anthropic Messages 格式（用于 Anthropic 兼容代理）

## 降级链

配置一个按优先级排列的降级链。当主服务商请求失败时，自动尝试列表中的下一个服务商：

```json
{
  "provider": "moonshot",
  "model": "kimi-2.5",
  "fallback_models": [
    {
      "provider": "deepseek",
      "model": "deepseek-chat",
      "api_key_env": "DEEPSEEK_API_KEY"
    },
    {
      "provider": "gemini",
      "model": "gemini-2.0-flash",
      "api_key_env": "GEMINI_API_KEY"
    }
  ]
}
```

**故障转移规则：**

- **401/403**（认证错误）-- 立即转移，不在同一服务商上重试
- **429**（限流）/ **5xx**（服务端错误）-- 指数退避重试，之后转移
- **400**（内容格式错误）-- 当错误包含 `"must not be empty"`、`"reasoning_content"`、`"API key not valid"` 或 `"invalid_value"` 时转移（不同服务商的验证规则可能不同）
- **超时** -- 立即转移（不在无响应的服务商上浪费 120s × 重试次数）
- **熔断器** -- 连续 3 次失败将标记该服务商为降级状态

**重试配置**（指数退避）：

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `max_retries` | 3 | 每个服务商的重试次数 |
| `initial_delay` | 1s | 首次重试延迟 |
| `max_delay` | 60s | 最大重试延迟 |
| `backoff_multiplier` | 2.0 | 指数倍增系数 |

429 错误会从响应体中解析 `"try again in Xs"` 以实现更智能的退避（回退默认：30s）。

## 自适应路由

当配置了多个降级模型时，自适应路由会以指标驱动的动态选择取代静态优先级链。

### 路由模式

三种互斥模式，通过 `adaptive_routing.mode` 设置：

| 模式 | 说明 |
|------|------|
| `off`（默认） | 静态优先级顺序。仅在熔断器打开（3 次连续失败）时才转移。 |
| `hedge` | **对冲竞速**：同时向 2 个服务商发送请求，取先到的结果，取消后到的。两者都累积 QoS 指标。 |
| `lane` | **评分选道**：基于 4 因子评分公式动态选择最优服务商。比对冲更省（无重复请求）。 |

```json
{
  "adaptive_routing": {
    "mode": "hedge",
    "qos_ranking": true,
    "latency_threshold_ms": 10000,
    "error_rate_threshold": 0.3,
    "probe_probability": 0.1,
    "probe_interval_secs": 60,
    "failure_threshold": 3
  }
}
```

| 配置项 | 默认值 | 说明 |
|---------|---------|-------------|
| `mode` | `"off"` | `"off"`、`"hedge"` 或 `"lane"` |
| `qos_ranking` | `false` | 启用 QoS 质量排名（使用模型目录评分） |
| `latency_threshold_ms` | 10000 | 内部使用的软惩罚阈值 |
| `error_rate_threshold` | 0.3 | 错误率超过此值的服务商将被降低优先级 |
| `probe_probability` | 0.1 | 发送至非主服务商作为健康探测的请求比例 |
| `probe_interval_secs` | 60 | 对同一服务商两次探测之间的最小间隔（秒） |
| `failure_threshold` | 3 | 连续失败多少次后触发熔断 |

### 评分公式（Lane 模式）

每个服务商通过加权 4 因子公式评分，**越低越好**。所有权重可通过 `adaptive_routing` 配置：

```
score = w_stability × blended_error_rate
      + w_quality  × (0.6 × norm_quality + 0.4 × norm_throughput)
      + w_priority × norm_config_order
      + w_cost      × norm_output_price
```

| 因子 | 权重键 | 默认 | 说明 |
|------|--------|------|------|
| 稳定性 | `weight_error_rate` | 0.3 | 混合基线 + 实时错误率。EMA 混合权重在 10 次调用中从 0 渐变到 1。 |
| 质量 | `weight_latency` | 0.3 | 60% 归一化 ds_output 质量 + 40% 归一化吞吐量（输出 tokens/秒 EMA） |
| 优先级 | `weight_priority` | 0.2 | 配置顺序偏好（0=主服务商，越高越靠后）。归一化到 [0, 1]。 |
| 成本 | `weight_cost` | 0.2 | 归一化的每百万 token 输出价格。未知成本 → 0（无惩罚）。 |

目录可以从 `model_catalog.json` 基准文件预填充，使路由器在启动时即具备参考评分而非冷启动启发。

### 自动升级

当检测到持续的延迟恶化时，会话 actor 会自动激活对冲模式 + 投机队列：

- `ResponsivenessObserver` 从前 5 次请求学习**中位数**基线（对异常值鲁棒），然后通过 80/20 EMA 每隔 20 个样本**自适应调整**基线。
- 如果连续 3 次 LLM 响应超过 **3×基线** 延迟，对冲竞速和投机队列同时启用。
- 当服务商恢复（一次正常延迟响应）时，两者都恢复为 Followup 和静态路由。

### 服务商包装栈

路由系统由分层包装器组成：

| 包装器 | 用途 |
|--------|------|
| `AdaptiveRouter` | 顶层：指标驱动评分、对冲/选道模式、熔断器、探测请求 |
| `ProviderChain` | 有序故障转移，带每服务商熔断器（失败次数 >= 阈值 → 降级） |
| `FallbackProvider` | 主服务商 + 按QoS排名的备选，通过 `ProviderRouter` 追踪冷却 |
| `RetryProvider` | 429/5xx 指数退避。超时 → 不重试（改为转移） |
| `ProviderRouter` | 子 Agent 多模型路由。前缀键解析、冷却、QoS评分备选 |
| `SwappableProvider` | 通过 `RwLock` 实现运行时模型切换（如 `switch_model` 工具）。每次切换泄漏约 50 字节 |
