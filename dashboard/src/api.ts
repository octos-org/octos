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
  User,
  SharedMetrics,
  MonitorStatus,
  SystemMetrics,
} from './types'

const BASE = '/api/admin'

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
    const text = await res.text()
    throw new Error(text || `HTTP ${res.status}`)
  }
  return res.json()
}

async function publicRequest<T>(path: string, opts?: RequestInit): Promise<T> {
  const res = await fetch(`/api${path}`, {
    headers: { 'Content-Type': 'application/json' },
    ...opts,
  })
  if (!res.ok) {
    const text = await res.text()
    throw new Error(text || `HTTP ${res.status}`)
  }
  return res.json()
}

async function authedRequest<T>(path: string, opts?: RequestInit): Promise<T> {
  const res = await fetch(`/api${path}`, {
    headers: getHeaders(),
    ...opts,
  })
  if (!res.ok) {
    const text = await res.text()
    throw new Error(text || `HTTP ${res.status}`)
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

  createSubAccount: (parentId: string, data: { name: string; email?: string; channels?: any[]; system_prompt?: string; env_vars?: Record<string, string> }) =>
    request<ProfileResponse>(`/profiles/${parentId}/accounts`, {
      method: 'POST',
      body: JSON.stringify(data),
    }),

  // User management (admin)
  listUsers: () => request<{ users: User[] }>('/users'),

  createUser: (data: { email: string; name: string; role?: string }) =>
    request<{ user: User }>('/users', {
      method: 'POST',
      body: JSON.stringify(data),
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

  logout: () =>
    authedRequest<ActionResponse>('/auth/logout', { method: 'POST' }),
}

// ── User self-service API (/api/my) ─────────────────────────────────

export const myApi = {
  getProfile: () => authedRequest<ProfileResponse>('/my/profile'),

  updateProfile: (data: {
    name?: string
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

  testSearch: (data: { provider: string; api_key?: string; api_key_env?: string }) =>
    authedRequest<{ ok: boolean; message?: string; error?: string }>('/my/test-search', {
      method: 'POST',
      body: JSON.stringify(data),
    }),

  listSubAccounts: () =>
    authedRequest<ProfileResponse[]>('/my/profile/accounts'),

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
