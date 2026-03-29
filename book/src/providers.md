# LLM Providers & Routing

Octos supports 14 LLM providers out of the box. Each provider needs an API key stored in an environment variable (except local providers like Ollama).

## Supported Providers

| Provider | Env Variable | Default Model | API Format | Aliases |
|----------|-------------|---------------|------------|---------|
| `anthropic` | `ANTHROPIC_API_KEY` | claude-sonnet-4-20250514 | Native Anthropic | -- |
| `openai` | `OPENAI_API_KEY` | gpt-4o | Native OpenAI | -- |
| `gemini` | `GEMINI_API_KEY` | gemini-2.0-flash | Native Gemini | -- |
| `openrouter` | `OPENROUTER_API_KEY` | anthropic/claude-sonnet-4-20250514 | Native OpenRouter | -- |
| `deepseek` | `DEEPSEEK_API_KEY` | deepseek-chat | OpenAI-compatible | -- |
| `groq` | `GROQ_API_KEY` | llama-3.3-70b-versatile | OpenAI-compatible | -- |
| `moonshot` | `MOONSHOT_API_KEY` | kimi-k2.5 | OpenAI-compatible | `kimi` |
| `dashscope` | `DASHSCOPE_API_KEY` | qwen-max | OpenAI-compatible | `qwen` |
| `minimax` | `MINIMAX_API_KEY` | MiniMax-Text-01 | OpenAI-compatible | -- |
| `zhipu` | `ZHIPU_API_KEY` | glm-4-plus | OpenAI-compatible | `glm` |
| `zai` | `ZAI_API_KEY` | glm-5 | Anthropic-compatible | `z.ai` |
| `nvidia` | `NVIDIA_API_KEY` | meta/llama-3.3-70b-instruct | OpenAI-compatible | `nim` |
| `ollama` | *(none)* | llama3.2 | OpenAI-compatible | -- |
| `vllm` | `VLLM_API_KEY` | *(must specify)* | OpenAI-compatible | -- |

## Configuration Methods

### Config File

Set `provider` and `model` in your `config.json`:

```json
{
  "provider": "moonshot",
  "model": "kimi-2.5",
  "api_key_env": "KIMI_API_KEY"
}
```

The `api_key_env` field overrides the default environment variable name for the provider. For example, Moonshot defaults to `MOONSHOT_API_KEY`, but you can point it at `KIMI_API_KEY` instead.

### CLI Flags

```bash
octos chat --provider deepseek --model deepseek-chat
octos chat --model gpt-4o  # auto-detects provider from model name
```

### Auth Store

Instead of environment variables, you can store API keys through the auth CLI:

```bash
# OAuth PKCE (OpenAI)
octos auth login --provider openai

# Device code flow (OpenAI)
octos auth login --provider openai --device-code

# Paste-token (all other providers)
octos auth login --provider anthropic
# -> prompts: "Paste your API key:"

# Check stored credentials
octos auth status

# Remove credentials
octos auth logout --provider openai
```

Credentials are stored in `~/.octos/auth.json` (file mode 0600). The auth store is checked **before** environment variables when resolving API keys.

## Auto-Detection

When `--provider` is omitted, Octos infers the provider from the model name:

| Model Pattern | Detected Provider |
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

## Custom Endpoints

Use `base_url` to point at self-hosted or proxy endpoints:

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

### API Type Override

The `api_type` field forces a specific wire format when a provider uses a non-standard protocol:

```json
{
  "provider": "zai",
  "model": "glm-5",
  "api_type": "anthropic"
}
```

- `"openai"` -- OpenAI Chat Completions format (default for most providers)
- `"anthropic"` -- Anthropic Messages format (for Anthropic-compatible proxies)

## Fallback Chains

Configure a priority-ordered fallback chain. If the primary provider fails, the next provider in the list is tried automatically:

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

**Failover rules:**

- **401/403** (authentication errors) -- failover immediately, no retry on the same provider
- **429** (rate limit) / **5xx** (server errors) -- retry with exponential backoff, then failover
- **400** (content-format errors) -- failover if the error contains "must not be empty", "reasoning_content", "API key not valid", or "invalid_value"
- **Timeouts** -- failover immediately, no retry (don't waste 120s × retries on an unresponsive provider)
- **Circuit breaker** -- 3 consecutive failures marks a provider as degraded

## Adaptive Routing

When multiple fallback models are configured, adaptive routing dynamically selects the best provider based on real-time performance metrics instead of following the static priority order. Three mutually exclusive modes are available:

```json
{
  "adaptive_routing": {
    "mode": "hedge",
    "qos_ranking": true,
    "latency_threshold_ms": 30000,
    "error_rate_threshold": 0.3,
    "probe_probability": 0.1,
    "probe_interval_secs": 60,
    "failure_threshold": 3,
    "weight_latency": 0.3,
    "weight_error_rate": 0.3,
    "weight_priority": 0.2,
    "weight_cost": 0.2
  }
}
```

### Adaptive Modes

| Mode | Description |
|------|-------------|
| `off` (default) | Static priority order. Failover only when a provider is circuit-broken (N consecutive failures). No scoring, no racing. |
| `hedge` | Hedged racing: fire each request to 2 providers simultaneously, take the winner, cancel the loser. Both results accumulate QoS metrics. |
| `lane` | Score-based lane changing: dynamically pick the best single provider based on a 4-factor scoring formula. Cheaper than hedge (no duplicate requests). |

### QoS Ranking

Setting `qos_ranking: true` enables quality-of-service ranking using a unified model catalog (`model_catalog.json`). The catalog provides baseline metrics (stability, latency, output quality) that blend with live traffic data via EMA:

- **Cold start**: Baseline catalog values are used (10 synthetic samples seeded).
- **Warm state**: Live metrics gradually replace baselines (weight ramps from 0 to 1 over 10 calls).
- **Export**: Live catalog is exported to `model_catalog.json` for observability.

### Scoring Formula

Each provider is scored on 4 factors (lower score = better). All weights are configurable via `adaptive_routing`:

| Factor | Weight key | Default | Description |
|--------|-----------|---------|-------------|
| **Stability** | `weight_error_rate` | 0.3 | Blended baseline + live error rate. EMA blend: weight ramps from 0→1 over 10 calls. |
| **Quality** | `weight_latency` | 0.3 | 60% normalized ds_output quality + 40% normalized throughput (output tokens/sec EMA) |
| **Priority** | `weight_priority` | 0.2 | Config-order preference (primary = 0). Normalize to [0, 1]. |
| **Cost** | `weight_cost` | 0.2 | Normalized output cost per million tokens. Unknown cost → 0 (no penalty). |

### Provider Metadata

| Setting | Default | Description |
|---------|---------|-------------|
| `latency_threshold_ms` | 30000 | Providers with average latency above this are penalized |
| `error_rate_threshold` | 0.3 | Providers with error rates above 30% are deprioritized |
| `probe_probability` | 0.1 | Fraction of requests sent to non-primary providers as health probes |
| `probe_interval_secs` | 60 | Minimum seconds between probes to the same provider |
| `failure_threshold` | 3 | Consecutive failures before the circuit breaker opens |

### Hedge Mode Details

When Hedge is active:
1. The primary provider and the cheapest alternate are raced via `tokio::select!`.
2. The winner's response is returned; the loser is cancelled.
3. Both completed requests record metrics (cancelled requests do not).
4. If the primary fails, the alternate is tried sequentially (it was cancelled by the race).

### Auto-Escalation

When sustained latency degradation is detected (3 consecutive responses exceeding 3× baseline), the session actor auto-activates Hedge mode + Speculative queue. The `ResponsivenessObserver` learns a **median** baseline from the first 5 requests (robust to outliers), then **adapts** every 20 samples via 80/20 EMA blend with the current window median. When the provider recovers (one normal-latency response), both revert to normal.

### Provider Wrappers

The routing stack is composed of layered wrappers:

| Wrapper | Purpose |
|---------|---------|
| `AdaptiveRouter` | Top-level: metrics-driven scoring, Hedge/Lane modes, circuit breaker, probe requests |
| `ProviderChain` | Ordered failover with per-provider circuit breaker (failure count ≥ threshold → degraded) |
| `FallbackProvider` | Primary + QoS-ranked fallbacks with cooldown tracking via `ProviderRouter` |
| `RetryProvider` | Exponential backoff on 429/5xx. Timeout → no retry (failover instead) |
| `ProviderRouter` | Sub-agent multi-model routing. Prefix-based key resolution, cooldown, QoS-scored fallbacks |
| `SwappableProvider` | Runtime model swap via `RwLock` (e.g. `switch_model` tool). Leaks ~50 bytes per swap |
