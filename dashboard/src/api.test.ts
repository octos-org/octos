import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { api } from './api'

beforeEach(() => {
  localStorage.clear()
})

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('dashboard api errors', () => {
  it('throws structured ApiError details for JSON error bodies', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(
        new Response(
          JSON.stringify({
            code: 'registered_email_exists',
            message: "email 'alice@example.com' is already registered",
          }),
          {
            status: 409,
            headers: { 'Content-Type': 'application/json' },
          },
        ),
      ),
    )

    await expect(api.addAllowedEmail({ email: 'alice@example.com' })).rejects.toMatchObject({
      name: 'ApiError',
      status: 409,
      code: 'registered_email_exists',
      message: "email 'alice@example.com' is already registered",
    })
  })

  it('keeps legacy text errors readable', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(new Response('HTTP 409', { status: 409 })),
    )

    await expect(api.addAllowedEmail({ email: 'alice@example.com' })).rejects.toMatchObject({
      name: 'ApiError',
      status: 409,
      message: 'HTTP 409',
    })
  })
})
