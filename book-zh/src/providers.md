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
- **熔断器** -- 连续 3 次失败将标记该服务商为降级状态

## 自适应路由

当配置了多个降级模型时，自适应路由会根据实时性能指标动态选择最优服务商，而非按静态优先级顺序。

```json
{
  "adaptive_routing": {
    "enabled": true,
    "latency_threshold_ms": 30000,
    "error_rate_threshold": 0.3,
    "probe_probability": 0.1,
    "probe_interval_secs": 60,
    "failure_threshold": 3
  }
}
```

| 配置项 | 默认值 | 说明 |
|---------|---------|-------------|
| `latency_threshold_ms` | 30000 | 平均延迟超过此值的服务商将被降权 |
| `error_rate_threshold` | 0.3 | 错误率超过 30% 的服务商将被降低优先级 |
| `probe_probability` | 0.1 | 发送至非主服务商作为健康探测的请求比例 |
| `probe_interval_secs` | 60 | 对同一服务商两次探测之间的最小间隔（秒） |
| `failure_threshold` | 3 | 连续失败多少次后触发熔断 |

启用自适应路由后，将以基于延迟和错误率的动态选择取代静态优先级链。少量请求会被路由到非主服务商作为探测，使系统能够检测恢复并重新平衡流量。
