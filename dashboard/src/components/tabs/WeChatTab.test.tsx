import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import WeChatTab from './WeChatTab'
import type { ProfileConfig } from '../../types'

const baseConfig: ProfileConfig = {
  channels: [],
  gateway: {},
  env_vars: {},
} as ProfileConfig

function qrStartResponse() {
  return new Response(
    JSON.stringify({ qrcode_url: 'https://example.test/qr', session_key: 'sk-1' }),
    { status: 200, headers: { 'Content-Type': 'application/json' } },
  )
}

function renderTab(profileId?: string) {
  const onChange = vi.fn()
  render(<WeChatTab config={baseConfig} onChange={onChange} profileId={profileId} />)
  return onChange
}

async function clickConnect() {
  await userEvent.click(screen.getByRole('button', { name: 'Connect WeChat' }))
}

describe('WeChatTab QR login routing', () => {
  beforeEach(() => {
    localStorage.clear()
  })

  afterEach(() => {
    vi.unstubAllGlobals()
  })

  it('uses the user-scoped route when viewing the own profile', async () => {
    const fetchMock = vi.fn().mockResolvedValue(qrStartResponse())
    vi.stubGlobal('fetch', fetchMock)
    renderTab()

    await clickConnect()

    await waitFor(() => expect(fetchMock).toHaveBeenCalled())
    expect(fetchMock.mock.calls[0][0]).toBe('/api/my/profile/wechat/qr-start')
  })

  it('uses the admin route for the profile being edited', async () => {
    const fetchMock = vi.fn().mockResolvedValue(qrStartResponse())
    vi.stubGlobal('fetch', fetchMock)
    renderTab('prof-a')

    await clickConnect()

    await waitFor(() => expect(fetchMock).toHaveBeenCalled())
    expect(fetchMock.mock.calls[0][0]).toBe('/api/admin/profiles/prof-a/wechat/qr-start')
  })

  it('URL-encodes the profile id in the admin route', async () => {
    const fetchMock = vi.fn().mockResolvedValue(qrStartResponse())
    vi.stubGlobal('fetch', fetchMock)
    renderTab('prof a/b')

    await clickConnect()

    await waitFor(() => expect(fetchMock).toHaveBeenCalled())
    expect(fetchMock.mock.calls[0][0]).toBe(
      '/api/admin/profiles/prof%20a%2Fb/wechat/qr-start',
    )
  })

  it('polls the same admin route with the session key', async () => {
    const fetchMock = vi.fn().mockImplementation((url: string) => {
      if (url.endsWith('/qr-start')) return Promise.resolve(qrStartResponse())
      return Promise.resolve(
        new Response(JSON.stringify({ status: 'wait' }), {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        }),
      )
    })
    vi.stubGlobal('fetch', fetchMock)
    renderTab('prof-a')

    await clickConnect()
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1))

    // First poll tick fires 2s after the QR session starts.
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2), {
      timeout: 4000,
    })
    const [pollUrl, pollInit] = fetchMock.mock.calls[1]
    expect(pollUrl).toBe('/api/admin/profiles/prof-a/wechat/qr-poll')
    expect(JSON.parse(String(pollInit?.body))).toEqual({ session_key: 'sk-1' })
  }, 10000)

  it('mirrors the server-pushed channel into local config on confirm', async () => {
    const fetchMock = vi.fn().mockImplementation((url: string) => {
      if (url.endsWith('/qr-start')) return Promise.resolve(qrStartResponse())
      return Promise.resolve(
        new Response(JSON.stringify({ status: 'confirmed', bot_id: 'bot-1' }), {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        }),
      )
    })
    vi.stubGlobal('fetch', fetchMock)
    const onChange = renderTab('prof-a')

    await clickConnect()
    await waitFor(() => expect(onChange).toHaveBeenCalled(), { timeout: 4000 })

    // A Save after the QR confirm now carries the wechat channel instead of
    // clobbering the server-pushed one with the stale array.
    const next = onChange.mock.calls[0][0] as ProfileConfig
    expect(next.channels.some((c) => c.type === 'wechat')).toBe(true)
    expect(next.env_vars.WECHAT_BOT_TOKEN).toBeUndefined()
  }, 10000)
})
