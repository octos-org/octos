export interface ProcessStatus {
  running: boolean
  pid: number | null
  started_at: string | null
  uptime_secs: number | null
}

export interface GatewaySettings {
  max_history?: number | null
  max_iterations?: number | null
  system_prompt?: string | null
  max_concurrent_sessions?: number | null
  browser_timeout_secs?: number | null
  max_output_tokens?: number | null
}

export interface ChannelCredentials {
  type: string
  [key: string]: string | number
}

export interface FallbackModel {
  provider: string
  model?: string | null
  base_url?: string | null
  api_key_env?: string | null
  api_type?: string | null
}

export interface EmailSettings {
  provider: string
  smtp_host?: string | null
  smtp_port?: number | null
  username?: string | null
  password_env?: string | null
  from_address?: string | null
  feishu_app_id?: string | null
  feishu_app_secret_env?: string | null
  feishu_from_address?: string | null
  feishu_region?: string | null
}

export interface HookConfig {
  event: string
  command: string[]
  timeout_ms?: number
  tool_filter?: string[]
}

export interface DockerConfig {
  image?: string | null
  cpu_limit?: string | null
  memory_limit?: string | null
  pids_limit?: number | null
}

export interface SandboxConfig {
  enabled?: boolean
  mode?: 'auto' | 'macos' | 'docker' | 'bwrap'
  allow_network?: boolean
  docker?: DockerConfig
}

export interface ProfileConfig {
  provider?: string | null
  model?: string | null
  base_url?: string | null
  api_key_env?: string | null
  api_type?: string | null
  fallback_models?: FallbackModel[]
  channels: ChannelCredentials[]
  gateway: GatewaySettings
  email?: EmailSettings | null
  env_vars: Record<string, string>
  hooks?: HookConfig[]
  admin_mode?: boolean
  sandbox?: SandboxConfig
}

export interface UserProfile {
  id: string
  name: string
  enabled: boolean
  data_dir: string | null
  parent_id?: string | null
  public_subdomain?: string | null
  config: ProfileConfig
  created_at: string
  updated_at: string
}

export interface ProfileResponse {
  id: string
  name: string
  enabled: boolean
  data_dir: string | null
  parent_id?: string | null
  public_subdomain?: string | null
  config: ProfileConfig
  created_at: string
  updated_at: string
  status: ProcessStatus
  email?: string | null
}

export interface OverviewResponse {
  total_profiles: number
  running: number
  stopped: number
  profiles: ProfileResponse[]
}

export interface ActionResponse {
  ok: boolean
  message?: string
}

export interface PurgeReport {
  profile_id: string
  user_email: string | null
  tenant_id: string | null
  node_name: string | null
  port_released: number | null
  files_removed: string[]
  bytes_freed: number
}

export interface BulkActionResponse {
  ok: boolean
  count: number
}

export type ChannelType = 'telegram' | 'discord' | 'slack' | 'whatsapp' | 'feishu' | 'email'

export const CHANNEL_TYPES: ChannelType[] = ['telegram', 'discord', 'slack', 'whatsapp', 'feishu', 'email']

export const CHANNEL_COLORS: Record<ChannelType, string> = {
  telegram: 'bg-blue-500',
  discord: 'bg-indigo-500',
  slack: 'bg-purple-500',
  whatsapp: 'bg-green-500',
  feishu: 'bg-cyan-500',
  email: 'bg-orange-500',
}

export const CHANNEL_LABELS: Record<ChannelType, string> = {
  telegram: 'TG',
  discord: 'DC',
  slack: 'SL',
  whatsapp: 'WA',
  feishu: 'FS',
  email: 'EM',
}

export const PROVIDERS = [
  'anthropic', 'openai', 'gemini', 'r9s', 'openrouter', 'deepseek',
  'groq', 'moonshot', 'dashscope', 'minimax', 'zhipu', 'zai',
  'nvidia', 'ollama', 'vllm',
] as const

// ── User & Auth types ───────────────────────────────────────────────

export type UserRole = 'admin' | 'user'

export interface User {
  id: string
  email: string
  name: string
  role: UserRole
  created_at: string
  last_login_at: string | null
}

export interface OtpSendResponse {
  ok: boolean
  message?: string
}

export interface OtpVerifyResponse {
  ok: boolean
  token?: string
  user?: User
  message?: string
}

export interface AllowlistEntry {
  email: string
  note?: string | null
  created_at: string
  claimed_user_id?: string | null
  claimed_at?: string | null
  registered: boolean
  registered_user_id?: string | null
  registered_name?: string | null
  last_login_at?: string | null
}

export interface MeResponse {
  user: User
  profile: ProfileResponse | null
}

export interface BridgeQrInfo {
  qr: string | null
  status: 'waiting' | 'connected' | 'disconnected' | 'logged_out'
  ws_port: number
  http_port: number
  phone_number: string | null
  lid: string | null
}

// ── Provider QoS Metrics ─────────────────────────────────────────────

export interface ProviderMetricsSnapshot {
  latency_ema_ms: number
  p95_latency_ms: number
  success_count: number
  failure_count: number
  consecutive_failures: number
  error_rate: number
}

export interface SharedProviderMetrics extends ProviderMetricsSnapshot {
  provider: string
  model: string
  score: number
}

export interface SharedPolicy {
  ema_alpha: number
  failure_threshold: number
  latency_threshold_ms: number
  error_rate_threshold: number
  probe_probability: number
  probe_interval_secs: number
  weight_latency: number
  weight_error_rate: number
  weight_priority: number
}

export interface SharedMetrics {
  updated_at: string
  policy: SharedPolicy
  providers: SharedProviderMetrics[]
}

// ── Admin Bot Config (legacy, kept for backwards compat) ─────────────

export interface AdminBotConfig {
  telegram_token_env?: string | null
  feishu_app_id_env?: string | null
  feishu_app_secret_env?: string | null
  admin_chat_ids: number[]
  admin_feishu_ids: string[]
  provider?: string | null
  model?: string | null
  base_url?: string | null
  api_key_env?: string | null
  alerts_enabled: boolean
  watchdog_enabled: boolean
  health_check_interval_secs: number
  max_restart_attempts: number
  env_vars: Record<string, string>
  fallback_models?: FallbackModel[]
}

// ── Monitor Status ──────────────────────────────────────────────────

export interface MonitorStatus {
  watchdog_enabled: boolean
  alerts_enabled: boolean
}

// ── System Metrics ─────────────────────────────────────────────────

export interface SystemMetrics {
  cpu: {
    usage_percent: number
    core_count: number
    brand: string
  }
  memory: {
    total_bytes: number
    used_bytes: number
    available_bytes: number
  }
  swap: {
    total_bytes: number
    used_bytes: number
  }
  disks: {
    name: string
    mount_point: string
    total_bytes: number
    available_bytes: number
    used_bytes: number
    file_system: string
  }[]
  top_processes: {
    pid: number
    name: string
    cpu_percent: number
    memory_bytes: number
  }[]
  platform: {
    hostname: string
    os: string
    os_version: string
    uptime_secs: number
  }
}
