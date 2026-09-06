# LLM Providers & Routing

Octos supports 17 LLM providers out of the box. Each provider needs an API key stored in an environment variable (except local providers like Ollama and Vertex AI, which uses a service-account JSON).

## Supported Providers

| Provider | Env Variable | Default Model | API Format | Aliases |
|----------|-------------|---------------|------------|---------|
| `anthropic` | `ANTHROPIC_API_KEY` | claude-sonnet-4-20250514 | Native Anthropic | -- |
| `openai` | `OPENAI_API_KEY` | gpt-4o | Native OpenAI | -- |
| `gemini` | `GEMINI_API_KEY` | gemini-2.5-flash | Native Gemini | `google` |
| `vertex` | `VERTEX_SA_JSON` | gemini-2.5-flash | Vertex AI (Gemini) | `vertex-ai`, `vertexai` |
| `openrouter` | `OPENROUTER_API_KEY` | anthropic/claude-sonnet-4-20250514 | Native OpenRouter | -- |
| `deepseek` | `DEEPSEEK_API_KEY` | deepseek-chat | OpenAI-compatible | -- |
| `groq` | `GROQ_API_KEY` | llama-3.3-70b-versatile | OpenAI-compatible | -- |
| `moonshot` | `MOONSHOT_API_KEY` | kimi-k2.5 | OpenAI-compatible | `kimi` |
| `dashscope` | `DASHSCOPE_API_KEY` | qwen-max | OpenAI-compatible | `qwen` |
| `minimax` | `MINIMAX_API_KEY` | MiniMax-M3 | OpenAI-compatible | -- |
| `minimax-cn` | `MINIMAX_CN_API_KEY` | MiniMax-M3 | OpenAI-compatible | `minimaxi` |
| `zhipu` | `ZHIPU_API_KEY` | glm-4-plus | OpenAI-compatible | `glm` |
| `zai` | `ZAI_API_KEY` | glm-5-turbo | Anthropic-compatible | `z.ai` |
| `r9s` | `R9S_API_KEY` | claude-sonnet-4-6 | Auto (Anthropic/OpenAI) | `r9s.ai` |
| `nvidia` | `NVIDIA_API_KEY` | meta/llama-3.3-70b-instruct | OpenAI-compatible | `nim` |
| `ollama` | *(none)* | llama3.2 | OpenAI-compatible | -- |
| `vllm` | `VLLM_API_KEY` | *(must specify)* | OpenAI-compatible | -- |

**`vertex`** authenticates with a Google service-account JSON (resolved via `VERTEX_SA_JSON` — keychain marker, config value, or env) instead of an API key; the GCP project is read from the JSON and the region is fixed to `global`. It must be selected explicitly (`provider: "vertex"`) — bare `gemini-*` model names still resolve to the AI Studio `gemini` provider. **`r9s`** is a multi-protocol proxy that auto-detects the Anthropic Messages API for `claude-*` models and OpenAI Chat Completions otherwise.

**`minimax-cn`** is the China region of MiniMax (`https://api.minimaxi.com/v1` instead of the international `https://api.minimax.io/v1`). MiniMax Token-plan subscription keys are issued by the China platform (platform.minimaxi.com) and are region-bound, so they only work against `minimax-cn`; international keys stay on `minimax`. MiniMax Coding-plan keys (`sk-cp-…`) additionally require the Anthropic protocol: choose protocol **Anthropic** during `octos init`, or set `api_type: "anthropic"` with `base_url: "https://api.minimaxi.com/anthropic"` — over the default OpenAI protocol they 401 (see octos#2115).

Any other OpenAI- or Anthropic-compatible endpoint (e.g. `wisemodel`, Together, Fireworks, Azure) is reachable by setting `base_url` on a provider — see [Custom Endpoints](#custom-endpoints).

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

Every provider setting is also an `octos chat` flag, for one-off runs without touching config:

```bash
# Known vendor (shorthand: supplies its default base URL + key env var)
octos chat --provider deepseek --model deepseek-chat

# Auto-detect the provider from the model name
octos chat --model gpt-4o

# Custom endpoint — name the real vendor, pick the wire protocol explicitly
octos chat --provider zai --api-type anthropic \
  --base-url https://api.z.ai/api/anthropic --model glm-5.2

# Full autonomy (bypass approvals + sandbox) alongside model selection
octos chat --yolo --provider zai --api-type anthropic \
  --base-url https://api.z.ai/api/anthropic --model glm-5.2
```

| Flag | Meaning |
|---|---|
| `--provider <name>` | Provider name (`anthropic`, `openai`, `zai`, `deepseek`, …). Supplies its default base URL + API-key env var. |
| `--model <name>` | Model to use. |
| `--base-url <url>` | Custom endpoint (overrides the provider default). |
| `--api-type <type>` (alias `--api-style`) | Wire protocol for `--base-url`: `anthropic`, `openai`, or `responses`. Overrides config's `api_type` — use this instead of overloading `--provider` with a vendor name. |
| `--yolo` (`--dangerously-bypass-approvals-and-sandbox`) | Full autonomy — no approvals, no sandbox. Local single-user only. |

CLI flags override config, which overrides the built-in default. The **API key is not a CLI flag** — it comes from the auth store, config, or an environment variable (see [Providing the API Key](#providing-the-api-key)).

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

### Providing the API Key

There is **no `--api-key` flag** — the key is resolved, in order:

1. **Auth store** — `octos auth login --provider <name>` (stored once, in `~/.octos/auth.json`).
2. **Config** — the `env_vars` map in `config.json` (below), or an `api_key_env` pointing at a variable.
3. **Environment variable** — whose name is the provider's default: `zai` → `ZAI_API_KEY`, `anthropic` → `ANTHROPIC_API_KEY`, `openai` → `OPENAI_API_KEY`, `deepseek` → `DEEPSEEK_API_KEY`, … (see the [table above](#supported-providers)).

```bash
# Quickest — export the provider's env var, then run
export ZAI_API_KEY=<your-key>
octos chat --provider zai --api-type anthropic \
  --base-url https://api.z.ai/api/anthropic --model glm-5.2

# Or log in once (no env var afterward)
octos auth login --provider zai      # prompts: "Paste your API key:"
octos auth status                    # which providers are logged in
octos auth keys                      # keys + keychain vs plaintext
```

Or bake it into `config.json` so nothing is needed at runtime:

```json
{
  "provider": "zai",
  "model": "glm-5.2",
  "base_url": "https://api.z.ai/api/anthropic",
  "api_type": "anthropic",
  "env_vars": { "ZAI_API_KEY": "<your-key>" }
}
```

> Passing a secret on the command line would land it in shell history and the process list; prefer `octos auth login`, an env var, or the config `env_vars` map.

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

`api_type` forces a specific wire protocol when a custom `base_url` speaks a known format under a non-matching provider name — so you name the real vendor with `provider` and pick the protocol with `api_type`, rather than overloading `provider` with a protocol name.

In config:

```json
{
  "provider": "zai",
  "model": "glm-5-turbo",
  "api_type": "anthropic"
}
```

Or on the command line with `--api-type` (alias `--api-style`), which overrides the config value:

```bash
octos chat --provider zai --api-type anthropic \
  --base-url https://api.z.ai/api/anthropic --model glm-5.2
```

- `"openai"` -- OpenAI Chat Completions format (default for most providers)
- `"anthropic"` -- Anthropic Messages format (for Anthropic-compatible proxies, e.g. z.ai/GLM)
- `"responses"` -- OpenAI Responses API format

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
      "model": "gemini-2.5-flash",
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
