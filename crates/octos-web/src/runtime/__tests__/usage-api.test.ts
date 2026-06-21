import { describe, expect, it, vi } from 'vitest';

import { UsageApiClient, type UsageAnalytics } from '../usage-api.js';

const emptyUsage: UsageAnalytics = {
  totals: {
    run_count: 0,
    input_tokens: 0,
    output_tokens: 0,
    estimated_cost_usd: 0,
  },
  by_day: [],
  by_month: [],
  by_profile: [],
  by_provider: [],
  by_model: [],
  by_channel: [],
};

function jsonResponse(body: unknown, status = 200) {
  return {
    ok: status >= 200 && status < 300,
    status,
    text: async () => JSON.stringify(body),
    json: async () => body,
  };
}

describe('UsageApiClient', () => {
  it('loads own session totals from the persistent usage API', async () => {
    const fetchImpl = vi.fn(async () => jsonResponse(emptyUsage));
    const client = new UsageApiClient({
      baseUrl: 'https://octos.example/',
      token: 'session-token',
      fetchImpl,
    });

    await client.mySessionUsage('session/a b', {
      from: '2026-05-01T00:00:00Z',
      to: new Date('2026-06-01T00:00:00Z'),
    });

    expect(fetchImpl).toHaveBeenCalledWith(
      'https://octos.example/api/my/usage/sessions/session%2Fa%20b?from=2026-05-01T00%3A00%3A00Z&to=2026-06-01T00%3A00%3A00.000Z',
      {
        headers: {
          Accept: 'application/json',
          Authorization: 'Bearer session-token',
        },
      },
    );
  });

  it('queries admin aggregate usage with optional session filters', async () => {
    const fetchImpl = vi.fn(async () => jsonResponse(emptyUsage));
    const client = new UsageApiClient({ fetchImpl });

    await client.adminUsage({ sessionId: 'thread-1' });

    expect(fetchImpl).toHaveBeenCalledWith('/api/admin/usage?session_id=thread-1', {
      headers: { Accept: 'application/json' },
    });
  });

  it('raises typed errors for non-2xx responses', async () => {
    const fetchImpl = vi.fn(async () => jsonResponse({ error: 'forbidden' }, 403));
    const client = new UsageApiClient({ fetchImpl });

    await expect(client.myUsage()).rejects.toMatchObject({
      name: 'UsageApiError',
      status: 403,
    });
  });
});
