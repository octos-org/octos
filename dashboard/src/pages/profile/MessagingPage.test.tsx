import { describe, it, expect, vi, afterEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import MessagingPage from './MessagingPage'
import type { ProfileConfig } from '../../types'

// #1440 regression: the page must hand the profile being edited down to
// WeChatTab — without it the QR flow silently falls back to /api/my and the
// token attaches to the wrong account.
const mockUseProfile = vi.fn()

vi.mock('../../contexts/ProfileContext', () => ({
  useProfile: () => mockUseProfile(),
}))

const baseConfig: ProfileConfig = {
  channels: [],
  gateway: {},
  env_vars: {},
} as ProfileConfig

function profileValue(isOwn: boolean, profileId: string) {
  return {
    config: baseConfig,
    setConfig: vi.fn(),
    save: vi.fn(),
    saving: false,
    loading: false,
    status: null,
    profileId,
    isOwn,
  }
}

function qrStartResponse() {
  return new Response(
    JSON.stringify({ qrcode_url: 'https://example.test/qr', session_key: 'sk-1' }),
    { status: 200, headers: { 'Content-Type': 'application/json' } },
  )
}

async function startWeChatLogin() {
  await userEvent.click(screen.getByRole('button', { name: 'WeChat' }))
  await userEvent.click(await screen.findByRole('button', { name: 'Connect WeChat' }))
}

describe('MessagingPage WeChat profile routing', () => {
  afterEach(() => {
    vi.unstubAllGlobals()
  })

  it('passes the edited profile down when an admin edits another profile', async () => {
    mockUseProfile.mockReturnValue(profileValue(false, 'prof-a'))
    const fetchMock = vi.fn().mockResolvedValue(qrStartResponse())
    vi.stubGlobal('fetch', fetchMock)
    render(<MessagingPage />)

    await startWeChatLogin()

    await waitFor(() => expect(fetchMock).toHaveBeenCalled())
    expect(fetchMock.mock.calls[0][0]).toBe('/api/admin/profiles/prof-a/wechat/qr-start')
  })

  it('uses the user-scoped route on the own profile page', async () => {
    mockUseProfile.mockReturnValue(profileValue(true, 'alice'))
    const fetchMock = vi.fn().mockResolvedValue(qrStartResponse())
    vi.stubGlobal('fetch', fetchMock)
    render(<MessagingPage />)

    await startWeChatLogin()

    await waitFor(() => expect(fetchMock).toHaveBeenCalled())
    expect(fetchMock.mock.calls[0][0]).toBe('/api/my/profile/wechat/qr-start')
  })
})
