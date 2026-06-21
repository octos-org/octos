// Typed client for persistent usage analytics.
//
// These endpoints read the backend ledger added for durable session totals,
// so chat surfaces can load accumulated usage after refresh, reconnect, or a
// server restart instead of trusting only the latest turn_completed envelope.

export type UsageCostSource = 'catalog_estimate' | 'provider_reported' | 'unavailable';

export interface UsageTotals {
  run_count: number;
  input_tokens: number;
  output_tokens: number;
  estimated_cost_usd: number;
}

export interface UsageRollup {
  key: string;
  totals: UsageTotals;
}

export interface UsageAnalytics {
  totals: UsageTotals;
  by_day: UsageRollup[];
  by_month: UsageRollup[];
  by_profile: UsageRollup[];
  by_provider: UsageRollup[];
  by_model: UsageRollup[];
  by_channel: UsageRollup[];
}

export interface UsageQueryParams {
  sessionId?: string;
  from?: string | Date;
  to?: string | Date;
}

type UsageFetchResponse = {
  ok: boolean;
  status: number;
  text(): Promise<string>;
  json(): Promise<unknown>;
};

type UsageFetch = (
  input: string,
  init?: { headers?: Record<string, string> },
) => Promise<UsageFetchResponse>;

export interface UsageApiClientOptions {
  baseUrl?: string;
  token?: string;
  fetchImpl?: UsageFetch;
}

export class UsageApiError extends Error {
  readonly status: number;

  constructor(status: number, message: string) {
    super(message || `HTTP ${status}`);
    this.name = 'UsageApiError';
    this.status = status;
  }
}

export class UsageApiClient {
  private readonly baseUrl: string;
  private readonly token?: string;
  private readonly fetchImpl: UsageFetch;

  constructor(options: UsageApiClientOptions = {}) {
    this.baseUrl = (options.baseUrl ?? '').replace(/\/+$/, '');
    this.token = options.token;
    this.fetchImpl = options.fetchImpl ?? defaultFetch();
  }

  myUsage(query: UsageQueryParams = {}): Promise<UsageAnalytics> {
    return this.get(`/api/my/usage${queryString(query)}`);
  }

  mySessionUsage(sessionId: string, query: Omit<UsageQueryParams, 'sessionId'> = {}) {
    return this.get(
      `/api/my/usage/sessions/${encodeURIComponent(sessionId)}${queryString(query)}`,
    );
  }

  adminUsage(query: UsageQueryParams = {}): Promise<UsageAnalytics> {
    return this.get(`/api/admin/usage${queryString(query)}`);
  }

  adminProfileUsage(profileId: string, query: UsageQueryParams = {}) {
    return this.get(
      `/api/admin/profiles/${encodeURIComponent(profileId)}/usage${queryString(query)}`,
    );
  }

  adminProfileSessionUsage(
    profileId: string,
    sessionId: string,
    query: Omit<UsageQueryParams, 'sessionId'> = {},
  ) {
    return this.get(
      `/api/admin/profiles/${encodeURIComponent(profileId)}/usage/sessions/${encodeURIComponent(
        sessionId,
      )}${queryString(query)}`,
    );
  }

  private async get(path: string): Promise<UsageAnalytics> {
    const headers: Record<string, string> = { Accept: 'application/json' };
    if (this.token) {
      headers.Authorization = `Bearer ${this.token}`;
    }
    const response = await this.fetchImpl(`${this.baseUrl}${path}`, { headers });
    if (!response.ok) {
      throw new UsageApiError(response.status, await response.text());
    }
    return (await response.json()) as UsageAnalytics;
  }
}

function defaultFetch(): UsageFetch {
  const fetchImpl = globalThis.fetch;
  if (typeof fetchImpl !== 'function') {
    throw new UsageApiError(0, 'global fetch is unavailable');
  }
  return fetchImpl as unknown as UsageFetch;
}

function queryString(query: UsageQueryParams): string {
  const params = new URLSearchParams();
  if (query.sessionId) {
    params.set('session_id', query.sessionId);
  }
  if (query.from) {
    params.set('from', timestampParam(query.from));
  }
  if (query.to) {
    params.set('to', timestampParam(query.to));
  }
  const encoded = params.toString();
  return encoded ? `?${encoded}` : '';
}

function timestampParam(value: string | Date): string {
  return value instanceof Date ? value.toISOString() : value;
}
