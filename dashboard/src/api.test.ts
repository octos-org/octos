// Tests for the ActionResponse ok-flag contract shared by the gateway
// action callers (ProfileContext, HomePage sub-account list).
//
// The self-service gateway routes report failures as HTTP 200 +
// `{ ok: false, message }` — only the admin ones use error statuses.

import { describe, it, expect } from 'vitest'
import { ensureActionOk } from './api'

describe('ensureActionOk', () => {
  it('passes an ok: true response through untouched', () => {
    expect(() => ensureActionOk({ ok: true }, 'Failed')).not.toThrow()
  })

  it('throws the backend message on ok: false', () => {
    expect(() => ensureActionOk({ ok: false, message: 'refused: reason' }, 'Failed')).toThrow('refused: reason')
  })

  it('falls back to the caller message when the refusal carries none', () => {
    expect(() => ensureActionOk({ ok: false }, 'Failed to start gateway')).toThrow('Failed to start gateway')
    expect(() => ensureActionOk({ ok: false, message: '' }, 'Failed to start gateway')).toThrow('Failed to start gateway')
  })
})
