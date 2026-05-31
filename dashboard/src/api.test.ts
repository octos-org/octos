import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { API_ERROR_CODES, ApiError, api } from './api'

const fetchMock = vi.fn()

function jsonResponse(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json' },
  })
}

beforeEach(() => {
  fetchMock.mockReset()
  localStorage.clear()
  vi.stubGlobal('fetch', fetchMock)
})

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('ApiError', () => {
  it('preserves structured server error codes and details', async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(409, {
        code: 'email_already_allowed',
        message: 'Email is already allowlisted',
        details: { email: 'dev@example.com' },
      }),
    )

    try {
      await api.addAllowedEmail({ email: 'dev@example.com' })
      throw new Error('expected request to fail')
    } catch (err) {
      expect(err).toBeInstanceOf(ApiError)
      const apiErr = err as ApiError
      expect(apiErr.status).toBe(409)
      expect(apiErr.code).toBe('email_already_allowed')
      expect(apiErr.message).toBe('Email is already allowlisted')
      expect(apiErr.details).toEqual({ email: 'dev@example.com' })
      expect(apiErr.payload).toEqual({
        code: 'email_already_allowed',
        message: 'Email is already allowlisted',
        details: { email: 'dev@example.com' },
      })
    }
  })

  it('derives typed codes for legacy text responses', async () => {
    fetchMock.mockResolvedValue(new Response('forbidden', { status: 403 }))

    try {
      await api.listUsers()
      throw new Error('expected request to fail')
    } catch (err) {
      expect(err).toBeInstanceOf(ApiError)
      const apiErr = err as ApiError
      expect(apiErr.status).toBe(403)
      expect(apiErr.code).toBe(API_ERROR_CODES.forbidden)
      expect(apiErr.message).toBe('forbidden')
      expect(ApiError.isAuthError(apiErr)).toBe(true)
    }
  })

  it('uses the HTTP status code when JSON errors omit code', async () => {
    fetchMock.mockResolvedValue(jsonResponse(422, { message: 'Invalid email' }))

    try {
      await api.addAllowedEmail({ email: 'not-an-email' })
      throw new Error('expected request to fail')
    } catch (err) {
      expect(err).toBeInstanceOf(ApiError)
      const apiErr = err as ApiError
      expect(apiErr.status).toBe(422)
      expect(apiErr.code).toBe(API_ERROR_CODES.validation)
      expect(apiErr.message).toBe('Invalid email')
    }
  })
})
