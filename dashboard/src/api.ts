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
  UserRole,
  AllowlistEntry,
  SharedMetrics,
  MonitorStatus,
  MonitorProfileStatus,
  SystemMetrics,
  PurgeReport,
  AdminAuditResponse,
  UsageAnalytics,
  UsageQueryParams,
} from './types'

const BASE = '/api/admin'

/// Error thrown by the dashboard's request helpers when the server returns a
/// non-2xx response. Carries the HTTP status code so callers can distinguish
/// auth failures (401/403) from infrastructure errors (5xx) without
/// re-parsing the message string. Use `ApiError.isAuthError(err)` for the
/// common "auth failed, hand back to the login flow" branch.
export class ApiError extends Error {
  public readonly status: number
  public readonly code?: string
  public readonly details?: unknown

  constructor(status: number, message: string, code?: string, details?: unknown) {
    super(message || `HTTP ${status}`)
    this.name = 'ApiError'
    this.status = status
    this.code = code
    this.details = details
  }

  static isAuthError(err: unknown): boolean {
    return err instanceof ApiError && (err.status === 401 || err.status === 403)
  }
}

type ErrorBody = {
  code?: unknown
  message?: unknown
  error?: unknown
  details?: unknown
}

async function apiErrorFromResponse(res: Response): Promise<ApiError> {
  const text = await res.text()
  if (text.trim()) {
    try {
      const body = JSON.parse(text) as ErrorBody
      if (body && typeof body === 'object') {
        const message = typeof body.message === 'string'
          ? body.message
          : typeof body.error === 'string'
            ? body.error
            : text
        const code = typeof body.code === 'string' ? body.code : undefined
        return new ApiError(res.status, message, code, body.details)
      }
    } catch {
      // Fall through to the raw response body for legacy text errors.
    }
  }
  return new ApiError(res.status, text)
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

function queryPath(path: string, params: Record<string, string | number | undefined | null>): string {
  const query = new URLSearchParams()
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined && value !== null && `${value}`.trim() !== '') {
      query.set(key, `${value}`)
    }
  }
  const suffix = query.toString()
  return suffix ? `${path}?${suffix}` : path
}

function usageQuery(query?: UsageQueryParams): string {
  const params = new URLSearchParams()
  if (query?.session_id) params.set('session_id', query.session_id)
  if (query?.from) params.set('from', query.from)
  if (query?.to) params.set('to', query.to)
  const encoded = params.toString()
  return encoded ? `?${encoded}` : ''
}

// ── Admin API (existing) ────────────────────────────────────────────

export const api = {
  overview: () => request<OverviewResponse>('/overview'),

  listAudit: (params?: {
    actor?: string
    action?: string
    target_id?: string
    from?: string
    to?: string
    limit?: number
    offset?: number
  }) => request<AdminAuditResponse>(queryPath('/audit', params ?? {})),

  listProfiles: (params?: { offset?: number; limit?: number }) =>
    request<ProfileResponse[]>(queryPath('/profiles', params ?? {})),

  // Fetch every profile page (the endpoint defaults to limit=100).
  listAllProfiles: async (): Promise<ProfileResponse[]> => {
    const pageSize = 100
    const all: ProfileResponse[] = []
    for (let offset = 0; ; offset += pageSize) {
      const page = await api.listProfiles({ offset, limit: pageSize })
      all.push(...page)
      if (page.length < pageSize) return all
    }
  },

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

  usage: (query?: UsageQueryParams) =>
    request<UsageAnalytics>(`/usage${usageQuery(query)}`),

  profileUsage: (id: string, query?: UsageQueryParams) =>
    request<UsageAnalytics>(`/profiles/${id}/usage${usageQuery(query)}`),

  profileSessionUsage: (id: string, sessionId: string, query?: Omit<UsageQueryParams, 'session_id'>) =>
    request<UsageAnalytics>(
      `/profiles/${id}/usage/sessions/${encodeURIComponent(sessionId)}${usageQuery(query)}`,
    ),

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

  updateUserRole: (id: string, role: UserRole) =>
    request<User>(`/users/${id}`, {
      method: 'PATCH',
      body: JSON.stringify({ role }),
    }),

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

  updateProfileMonitor: (
    id: string,
    data: { watchdog?: 'inherit' | 'enabled' | 'disabled'; alerts?: 'inherit' | 'enabled' | 'disabled' },
  ) =>
    request<MonitorProfileStatus>(`/monitor/profiles/${encodeURIComponent(id)}`, {
      method: 'POST',
      body: JSON.stringify(data),
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

  usage: (query?: UsageQueryParams) =>
    authedRequest<UsageAnalytics>(`/my/usage${usageQuery(query)}`),

  sessionUsage: (sessionId: string, query?: Omit<UsageQueryParams, 'session_id'>) =>
    authedRequest<UsageAnalytics>(
      `/my/usage/sessions/${encodeURIComponent(sessionId)}${usageQuery(query)}`,
    ),

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
