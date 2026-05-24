import { beforeEach, describe, expect, it, vi } from 'vitest'
import { ApiError, api } from './api'

describe('api error handling', () => {
  beforeEach(() => {
    localStorage.clear()
    vi.restoreAllMocks()
  })

  it('preserves structured error code, message, and details', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response(
        JSON.stringify({
          code: 'email_already_allowed',
          message: 'Email "user@example.com" is already allowlisted',
          details: { email: 'user@example.com' },
        }),
        {
          status: 409,
          headers: { 'Content-Type': 'application/json' },
        },
      ),
    )

    await expect(
      api.addAllowedEmail({ email: 'user@example.com' }),
    ).rejects.toMatchObject({
      status: 409,
      code: 'email_already_allowed',
      message: 'Email "user@example.com" is already allowlisted',
      details: { email: 'user@example.com' },
    })
  })

  it('falls back to plain text response bodies', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response('legacy conflict', { status: 409 }),
    )

    await expect(
      api.addAllowedEmail({ email: 'user@example.com' }),
    ).rejects.toMatchObject({
      status: 409,
      code: 'http_error',
      message: 'legacy conflict',
    })
  })

  it('keeps ApiError auth detection backward compatible', () => {
    expect(ApiError.isAuthError(new ApiError(401, 'unauthorized'))).toBe(true)
    expect(ApiError.isAuthError(new ApiError(500, 'server error'))).toBe(false)
  })
})
