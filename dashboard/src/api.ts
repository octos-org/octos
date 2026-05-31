import type {
  ProfileResponse,
  OverviewResponse,
  ActionResponse,
  BulkActionResponse,
  BridgeQrInfo,
  ProfileConfig,
  OtpSendResponse,
  OtpVerifyResponse,
  MeResponse,
  AuthStatusResponse,
  SoloLoginResult,
  SoloCreateResult,
  User,
  AllowlistEntry,
  SharedMetrics,
  MonitorStatus,
  SystemMetrics,
  PurgeReport,
} from './types'

const BASE = '/api/admin'

export const API_ERROR_CODES = {
  unauthorized: 'unauthorized',
  forbidden: 'forbidden',
  notFound: 'not_found',
  conflict: 'conflict',
  validation: 'validation_error',
  rateLimited: 'rate_limited',
  server: 'server_error',
  http: 'http_error',
} as const

export type KnownApiErrorCode =
  (typeof API_ERROR_CODES)[keyof typeof API_ERROR_CODES]

export type ApiErrorCode = KnownApiErrorCode | (string & {})

export interface ApiErrorPayload<C extends ApiErrorCode = ApiErrorCode> {
  code: C
  message: string
  details?: unknown
}

interface ApiErrorOptions<C extends ApiErrorCode = ApiErrorCode> {
  code?: C
  details?: unknown
  payload?: ApiErrorPayload<C>
}

/// Error thrown by the dashboard's request helpers when the server returns a
/// non-2xx response. Carries both HTTP status and structured server error code
/// so callers can branch on `err.code` instead of parsing message text.
export class ApiError<C extends ApiErrorCode = ApiErrorCode> extends Error {
  public readonly status: number
  public readonly code: C
  public readonly details?: unknown
  public readonly payload: ApiErrorPayload<C>

  constructor(
    status: number,
    message: string,
    codeOrOptions?: C | ApiErrorOptions<C>,
    details?: unknown,
  ) {
    const options: ApiErrorOptions<C> =
      typeof codeOrOptions === 'object' && codeOrOptions !== null
        ? codeOrOptions
        : { code: codeOrOptions, details }
    const code = options.code ?? (apiErrorCodeForStatus(status) as C)
    const resolvedMessage = message || `HTTP ${status}`
    super(resolvedMessage)
    this.name = 'ApiError'
    this.status = status
    this.code = code
    this.details = options.details
    this.payload = options.payload ?? {
      code,
      message: resolvedMessage,
      ...(options.details === undefined ? {} : { details: options.details }),
    }
  }

  static isAuthError(err: unknown): boolean {
    return (
      err instanceof ApiError &&
      (err.code === API_ERROR_CODES.unauthorized ||
        err.code === API_ERROR_CODES.forbidden ||
        err.status === 401 ||
        err.status === 403)
    )
  }
}

function apiErrorCodeForStatus(status: number): KnownApiErrorCode {
  if (status === 401) return API_ERROR_CODES.unauthorized
  if (status === 403) return API_ERROR_CODES.forbidden
  if (status === 404) return API_ERROR_CODES.notFound
  if (status === 409) return API_ERROR_CODES.conflict
  if (status === 422 || status === 400) return API_ERROR_CODES.validation
  if (status === 429) return API_ERROR_CODES.rateLimited
  if (status >= 500) return API_ERROR_CODES.server
  return API_ERROR_CODES.http
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function normalizeApiErrorPayload(status: number, raw: string): ApiErrorPayload {
  const fallbackCode = apiErrorCodeForStatus(status)
  const fallbackMessage = raw.trim() || `HTTP ${status}`

  if (!raw.trim()) {
    return { code: fallbackCode, message: fallbackMessage }
  }

  try {
    const parsed: unknown = JSON.parse(raw)
    if (!isRecord(parsed)) {
      return { code: fallbackCode, message: fallbackMessage }
    }
    const code =
      typeof parsed.code === 'string' && parsed.code.trim()
        ? parsed.code
        : fallbackCode
    const message =
      typeof parsed.message === 'string' && parsed.message.trim()
        ? parsed.message
        : typeof parsed.error === 'string' && parsed.error.trim()
          ? parsed.error
          : fallbackMessage
    return {
      code,
      message,
      ...(Object.prototype.hasOwnProperty.call(parsed, 'details')
        ? { details: parsed.details }
        : {}),
    }
  } catch {
    return { code: fallbackCode, message: fallbackMessage }
  }
}

async function apiErrorFromResponse(res: Response): Promise<ApiError> {
  const raw = await res.text()
  const payload = normalizeApiErrorPayload(res.status, raw)
  return new ApiError(res.status, payload.message, {
    code: payload.code,
    details: payload.details,
    payload,
  })
}

export interface SkillRegistryPackage {
  name: string
  description: string
  repo: string
  version: string | null
  author: string | null
  license: string | null
  skills: string[]
  requires: string[]
  provides_tools: boolean
  tags: string[]
}

function getHeaders(): HeadersInit {
  const headers: HeadersInit = { 'Content-Type': 'application/json' }
  const token = localStorage.getItem('octos_session_token')
    || localStorage.getItem('octos_auth_token')
  if (token) {
    headers['Authorization'] = `Bearer ${token}`
  }
  return headers
}

async function request<T>(path: string, opts?: RequestInit): Promise<T> {
  const res = await fetch(`${BASE}${path}`, {
    headers: getHeaders(),
    ...opts,
  })
  if (!res.ok) {
    throw await apiErrorFromResponse(res)
  }
  return res.json()
}

async function requestNoContent(path: string, opts?: RequestInit): Promise<void> {
  const res = await fetch(`${BASE}${path}`, {
    headers: getHeaders(),
    ...opts,
  })
  if (!res.ok) {
    throw await apiErrorFromResponse(res)
  }
}

async function publicRequest<T>(path: string, opts?: RequestInit): Promise<T> {
  const res = await fetch(`/api${path}`, {
    headers: { 'Content-Type': 'application/json' },
    ...opts,
  })
  if (!res.ok) {
    throw await apiErrorFromResponse(res)
  }
  return res.json()
}

async function authedRequest<T>(path: string, opts?: RequestInit): Promise<T> {
  const res = await fetch(`/api${path}`, {
    headers: getHeaders(),
    ...opts,
  })
  if (!res.ok) {
    throw await apiErrorFromResponse(res)
  }
  return res.json()
}

// ── Admin API (existing) ────────────────────────────────────────────

export const api = {
  overview: () => request<OverviewResponse>('/overview'),

  listProfiles: () => request<ProfileResponse[]>('/profiles'),

  getProfile: (id: string) => request<ProfileResponse>(`/profiles/${id}`),

  createProfile: (data: {
    id: string
    name: string
    public_subdomain?: string | null
    enabled?: boolean
    data_dir?: string | null
    config?: ProfileConfig
  }) =>
    request<ProfileResponse>('/profiles', {
      method: 'POST',
      body: JSON.stringify(data),
    }),

  updateProfile: (
    id: string,
    data: {
      name?: string
      public_subdomain?: string | null
      enabled?: boolean
      data_dir?: string | null
      config?: ProfileConfig
    },
  ) =>
    request<ProfileResponse>(`/profiles/${id}`, {
      method: 'PUT',
      body: JSON.stringify(data),
    }),

  deleteProfile: (id: string) =>
    request<ActionResponse>(`/profiles/${id}`, { method: 'DELETE' }),

  purgeProfile: (id: string) =>
    request<PurgeReport>(`/profiles/${id}/purge`, { method: 'POST' }),

  startGateway: (id: string) =>
    request<ActionResponse>(`/profiles/${id}/start`, { method: 'POST' }),

  stopGateway: (id: string) =>
    request<ActionResponse>(`/profiles/${id}/stop`, { method: 'POST' }),

  restartGateway: (id: string) =>
    request<ActionResponse>(`/profiles/${id}/restart`, { method: 'POST' }),

  startAll: () => request<BulkActionResponse>('/start-all', { method: 'POST' }),

  stopAll: () => request<BulkActionResponse>('/stop-all', { method: 'POST' }),

  whatsappQr: (id: string) =>
    request<BridgeQrInfo>(`/profiles/${id}/whatsapp/qr`),

  providerMetrics: (id: string) =>
    request<SharedMetrics | null>(`/profiles/${id}/metrics`),

  // Sub-account management
  listSubAccounts: (parentId: string) =>
    request<ProfileResponse[]>(`/profiles/${parentId}/accounts`),

  createSubAccount: (parentId: string, data: { sub_account_id?: string; name: string; public_subdomain?: string | null; email?: string; channels?: any[]; system_prompt?: string; env_vars?: Record<string, string> }) =>
    request<ProfileResponse>(`/profiles/${parentId}/accounts`, {
      method: 'POST',
      body: JSON.stringify(data),
    }),

  // User management (admin)
  listUsers: () => request<{ users: User[] }>('/users'),

  listAllowedEmails: () => request<{ entries: AllowlistEntry[] }>('/allowed-emails'),

  addAllowedEmail: (data: { email: string; note?: string }) =>
    request<AllowlistEntry>('/allowed-emails', {
      method: 'POST',
      body: JSON.stringify(data),
    }),

  deleteAllowedEmail: (email: string) =>
    request<ActionResponse>(`/allowed-emails/${encodeURIComponent(email)}`, {
      method: 'DELETE',
    }),

  deleteUser: (id: string) =>
    request<ActionResponse>(`/users/${id}`, { method: 'DELETE' }),

  // Monitor control
  monitorStatus: () => request<MonitorStatus>('/monitor/status'),

  toggleWatchdog: (enabled: boolean) =>
    request<{ ok: boolean; watchdog_enabled: boolean }>('/monitor/watchdog', {
      method: 'POST',
      body: JSON.stringify({ enabled }),
    }),

  toggleAlerts: (enabled: boolean) =>
    request<{ ok: boolean; alerts_enabled: boolean }>('/monitor/alerts', {
      method: 'POST',
      body: JSON.stringify({ enabled }),
    }),

  gatewayStatus: (id: string) =>
    request<{ running: boolean; pid: number | null }>(`/profiles/${id}/status`),

  systemMetrics: (opts?: { procs?: boolean }) =>
    request<SystemMetrics>(`/system/metrics${opts?.procs ? '?procs=1' : ''}`),

  // Skills management
  listProfileSkills: (id: string) =>
    request<{ skills: { name: string; version: string | null; tool_count: number; source_repo: string | null }[] }>(
      `/profiles/${id}/skills`,
    ),

  installProfileSkill: (id: string, data: { repo: string; force: boolean; branch: string }) =>
    request<{ ok: boolean; installed: string[]; skipped: string[]; deps_installed: boolean }>(
      `/profiles/${id}/skills`,
      { method: 'POST', body: JSON.stringify(data) },
    ),

  removeProfileSkill: (id: string, name: string) =>
    request<ActionResponse>(`/profiles/${id}/skills/${name}`, { method: 'DELETE' }),

  // Setup wizard + admin token rotation
  getTokenStatus: () => request<TokenStatus>('/token/status'),

  rotateToken: (new_token: string) =>
    requestNoContent('/token/rotate', {
      method: 'POST',
      body: JSON.stringify({ new_token }),
    }),

  emailToken: (to: string, token: string) =>
    request<EmailTokenResult>('/token/email', {
      method: 'POST',
      body: JSON.stringify({ to, token }),
    }),

  getSetupState: () => request<SetupState>('/setup/state'),

  postSetupStep: (step: number) =>
    requestNoContent('/setup/step', {
      method: 'POST',
      body: JSON.stringify({ step }),
    }),

  completeSetup: () => requestNoContent('/setup/complete', { method: 'POST' }),

  skipSetup: () => requestNoContent('/setup/skip', { method: 'POST' }),

  // SMTP configuration (used by the setup wizard and future Settings pages)
  getSmtp: () => request<SmtpSettings>('/smtp'),

  saveSmtp: (data: SmtpSettingsBody) =>
    requestNoContent('/smtp', {
      method: 'POST',
      body: JSON.stringify(data),
    }),

  testSmtp: (to: string) =>
    request<SmtpTestResult>('/smtp/test', {
      method: 'POST',
      body: JSON.stringify({ to }),
    }),

  // Deployment mode (local / tenant / cloud)
  getDeploymentMode: () => request<DeploymentModeBody>('/deployment-mode'),

  saveDeploymentMode: (mode: DeploymentMode) =>
    requestNoContent('/deployment-mode', {
      method: 'POST',
      body: JSON.stringify({ mode }),
    }),

  detectDeploymentMode: () =>
    request<DeploymentModeDetection>('/deployment-mode/detect'),

  testProvider: (data: {
    provider: string
    model: string
    api_key?: string
    api_key_env?: string
    base_url?: string
  }) =>
    request<{ ok: boolean; message?: string; error?: string }>('/test-provider', {
      method: 'POST',
      body: JSON.stringify(data),
    }),
}

// ── Setup wizard types ─────────────────────────────────────────────

export type TokenStatus = { rotated: boolean }

export type SetupState = {
  wizard_completed_at: string | null
  wizard_skipped: boolean
  wizard_last_step_reached: number
}

// ── SMTP + deployment-mode types ───────────────────────────────────

export type SmtpSettings = {
  host: string
  port: number
  username: string
  from_address: string
  password_configured: boolean
  allow_self_registration: boolean
}

export type SmtpSettingsBody = {
  host: string
  port: number
  username: string
  from_address: string
  /** Leave undefined / empty to keep the existing password. */
  password?: string
  /** Omit to leave the current setting alone; pass true/false to write
   *  dashboard_auth.allow_self_registration in config.json and hot-reload
   *  the in-memory AuthManager flag. */
  allow_self_registration?: boolean
}

export type SmtpTestResult = {
  ok: boolean
  message?: string
  error?: string
}

export type EmailTokenResult = {
  ok: boolean
  message?: string
  error?: string
}

export type DeploymentMode = 'local' | 'tenant' | 'cloud'

export type DeploymentModeBody = { mode: DeploymentMode; explicit?: boolean }

export type DeploymentModeDetection = { detected: DeploymentMode }

// ── Server info (public) ─────────────────────────────────────────────

export interface ServerStatus {
  version: string
  model: string
  provider: string
  uptime_secs: number
  agent_configured: boolean
  /** Public-facing base domain this mini serves profiles under
   *  (e.g. `"crew.ominix.io"`, `"bot.ominix.io"`). Always a concrete
   *  string — the server substitutes `"crew.ominix.io"` when
   *  unconfigured. */
  base_domain: string
}

export const systemApi = {
  status: () => authedRequest<ServerStatus>('/status'),
}

// ── Auth API (public) ───────────────────────────────────────────────

export const authApi = {
  sendCode: (email: string) =>
    publicRequest<OtpSendResponse>('/auth/send-code', {
      method: 'POST',
      body: JSON.stringify({ email }),
    }),

  verify: (email: string, code: string) =>
    publicRequest<OtpVerifyResponse>('/auth/verify', {
      method: 'POST',
      body: JSON.stringify({ email, code }),
    }),

  me: () => authedRequest<MeResponse>('/auth/me'),

  // Public login configuration — which login modes to render.
  status: () => publicRequest<AuthStatusResponse>('/auth/status'),

  // No-password solo login (Local-mode host, loopback peer). `soloLogin`
  // re-logs the existing owner (404 when none exists yet); `soloCreate`
  // onboards a local profile and logs in atomically.
  soloLogin: () =>
    publicRequest<SoloLoginResult>('/auth/solo', { method: 'POST' }),

  soloCreate: (body: { name: string; username: string; email: string }) =>
    publicRequest<SoloCreateResult>('/auth/solo/create', {
      method: 'POST',
      body: JSON.stringify(body),
    }),

  logout: () =>
    authedRequest<ActionResponse>('/auth/logout', { method: 'POST' }),
}

// ── User self-service API (/api/my) ─────────────────────────────────

export const myApi = {
  getProfile: () => authedRequest<ProfileResponse>('/my/profile'),

  updateProfile: (data: {
    name?: string
    email?: string
    public_subdomain?: string | null
    enabled?: boolean
    config?: ProfileConfig
  }) =>
    authedRequest<ProfileResponse>('/my/profile', {
      method: 'PUT',
      body: JSON.stringify(data),
    }),

  startGateway: () =>
    authedRequest<ActionResponse>('/my/profile/start', { method: 'POST' }),

  stopGateway: () =>
    authedRequest<ActionResponse>('/my/profile/stop', { method: 'POST' }),

  restartGateway: () =>
    authedRequest<ActionResponse>('/my/profile/restart', { method: 'POST' }),

  gatewayStatus: () =>
    authedRequest<{ running: boolean; pid: number | null }>('/my/profile/status'),

  whatsappQr: () =>
    authedRequest<BridgeQrInfo>('/my/profile/whatsapp/qr'),

  providerMetrics: () =>
    authedRequest<SharedMetrics | null>('/my/profile/metrics'),

  listProfileSkills: () =>
    authedRequest<{ skills: { name: string; version: string | null; tool_count: number; source_repo: string | null }[] }>(
      '/my/profile/skills',
    ),

  listProfileSkillRegistry: (query?: string) =>
    authedRequest<{ packages: SkillRegistryPackage[] }>(
      `/my/profile/skills/registry${query ? `?q=${encodeURIComponent(query)}` : ''}`,
    ),

  installProfileSkill: (data: { repo: string; force: boolean; branch: string }) =>
    authedRequest<{ ok: boolean; installed: string[]; skipped: string[]; deps_installed: boolean }>(
      '/my/profile/skills',
      { method: 'POST', body: JSON.stringify(data) },
    ),

  removeProfileSkill: (name: string) =>
    authedRequest<ActionResponse>(`/my/profile/skills/${name}`, { method: 'DELETE' }),

  testProvider: (data: { provider: string; model: string; api_key?: string; api_key_env?: string; base_url?: string }) =>
    authedRequest<{ ok: boolean; message?: string; error?: string; models?: string[] }>('/my/test-provider', {
      method: 'POST',
      body: JSON.stringify(data),
    }),

  fetchProviderModels: (data: { provider: string; model?: string; api_key?: string; api_key_env?: string; base_url?: string; profile_id?: string }) =>
    authedRequest<string[]>('/my/provider-models', {
      method: 'POST',
      body: JSON.stringify(data),
    }),

  testSearch: (data: { provider: string; api_key?: string; api_key_env?: string; profile_id?: string }) =>
    authedRequest<{ ok: boolean; message?: string; error?: string }>('/my/test-search', {
      method: 'POST',
      body: JSON.stringify(data),
    }),

  listSubAccounts: () =>
    authedRequest<ProfileResponse[]>('/my/profile/accounts'),

  getSubAccount: (id: string) =>
    authedRequest<ProfileResponse>(`/my/profile/accounts/${id}`),

  createSubAccount: (data: { sub_account_id?: string; name: string; public_subdomain?: string | null; email?: string; channels?: any[]; system_prompt?: string; env_vars?: Record<string, string> }) =>
    authedRequest<ProfileResponse>('/my/profile/accounts', {
      method: 'POST',
      body: JSON.stringify(data),
    }),

  updateSubAccount: (
    id: string,
    data: {
      name?: string
      public_subdomain?: string | null
      enabled?: boolean
      config?: ProfileConfig
      email?: string
    },
  ) =>
    authedRequest<ProfileResponse>(`/my/profile/accounts/${id}`, {
      method: 'PUT',
      body: JSON.stringify(data),
    }),

  startSubGateway: (id: string) =>
    authedRequest<ActionResponse>(`/my/profile/accounts/${id}/start`, { method: 'POST' }),

  stopSubGateway: (id: string) =>
    authedRequest<ActionResponse>(`/my/profile/accounts/${id}/stop`, { method: 'POST' }),
}

// Helper to get SSE log URL with auth token (user's own profile)
export function getLogStreamUrl(): string {
  const token = localStorage.getItem('octos_session_token')
    || localStorage.getItem('octos_auth_token')
  const base = `/api/my/profile/logs`
  return token ? `${base}?token=${encodeURIComponent(token)}` : base
}

export function getAdminLogStreamUrl(profileId: string): string {
  const token = localStorage.getItem('octos_session_token')
    || localStorage.getItem('octos_auth_token')
  const base = `/api/admin/profiles/${profileId}/logs`
  return token ? `${base}?token=${encodeURIComponent(token)}` : base
}
