// Tests for the gateway lifecycle actions on ProfileContext.
//
// The self-service gateway routes (`/my/profile/*`, sub-account start/stop)
// report failures as HTTP 200 + `{ ok: false, message }` — only the admin
// ones use error statuses. The context must consume the `ok` flag itself,
// otherwise a refusal (e.g. the solo in-process serve guard) surfaces as a
// "Gateway started" success toast.

import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter } from 'react-router-dom'
import { ProfileProvider, useProfile } from './ProfileContext'
import { ToastProvider } from '../components/Toast'

const { mockMyStart, mockMyStop, mockMyRestart } = vi.hoisted(() => ({
  mockMyStart: vi.fn(),
  mockMyStop: vi.fn(),
  mockMyRestart: vi.fn(),
}))

vi.mock('../api', () => ({
  api: {},
  myApi: {
    getProfile: () =>
      Promise.resolve({
        id: 'prof-1',
        name: 'Prof One',
        email: 'owner@example.com',
        public_subdomain: 'prof-1',
        enabled: true,
        parent_id: null,
        status: null,
        config: {},
      }),
    startGateway: () => mockMyStart(),
    stopGateway: () => mockMyStop(),
    restartGateway: () => mockMyRestart(),
  },
  getLogStreamUrl: () => '/api/my/profile/logs',
  getAdminLogStreamUrl: (id: string) => `/api/profiles/${id}/logs`,
}))

vi.mock('./AuthContext', () => ({
  useAuth: () => ({
    user: { id: 'user-1', name: 'Owner', email: 'owner@example.com' },
    isAdmin: false,
    scopedProfile: null,
  }),
}))

function GatewayButtons() {
  const { startGateway, stopGateway, restartGateway } = useProfile()
  return (
    <div>
      <button data-testid="start" onClick={() => void startGateway()}>start</button>
      <button data-testid="stop" onClick={() => void stopGateway()}>stop</button>
      <button data-testid="restart" onClick={() => void restartGateway()}>restart</button>
    </div>
  )
}

function renderProvider() {
  return render(
    <MemoryRouter>
      <ToastProvider>
        <ProfileProvider>
          <GatewayButtons />
        </ProfileProvider>
      </ToastProvider>
    </MemoryRouter>,
  )
}

beforeEach(() => {
  mockMyStart.mockReset()
  mockMyStop.mockReset()
  mockMyRestart.mockReset()
  mockMyStart.mockResolvedValue({ ok: true })
  mockMyStop.mockResolvedValue({ ok: true })
  mockMyRestart.mockResolvedValue({ ok: true })
})

describe('ProfileContext gateway actions', () => {
  it('shows a solo-guard refusal as an error toast instead of "Gateway started"', async () => {
    const user = userEvent.setup()
    mockMyStart.mockResolvedValue({
      ok: false,
      message: "gateway start refused for profile 'prof-1': restart the serve without --solo to run profile gateways",
    })
    renderProvider()

    await user.click(screen.getByTestId('start'))

    await waitFor(() => {
      expect(screen.getByText(/restart the serve without --solo/)).toBeTruthy()
    })
    expect(screen.queryByText('Gateway started')).toBeNull()
  })

  it('shows a failed self-service restart as an error toast instead of "Gateway restarted"', async () => {
    const user = userEvent.setup()
    mockMyRestart.mockResolvedValue({ ok: false, message: 'gateway start refused for profile \'prof-1\'' })
    renderProvider()

    await user.click(screen.getByTestId('restart'))

    await waitFor(() => {
      expect(screen.getByText(/gateway start refused for profile 'prof-1'/)).toBeTruthy()
    })
    expect(screen.queryByText('Gateway restarted')).toBeNull()
  })

  it('shows a failed self-service stop as an error toast instead of "Gateway stopped"', async () => {
    const user = userEvent.setup()
    mockMyStop.mockResolvedValue({ ok: false, message: 'Gateway not running' })
    renderProvider()

    await user.click(screen.getByTestId('stop'))

    await waitFor(() => {
      expect(screen.getByText('Gateway not running')).toBeTruthy()
    })
    expect(screen.queryByText('Gateway stopped')).toBeNull()
  })

  it('still toasts success when the gateway action reports ok: true', async () => {
    const user = userEvent.setup()
    renderProvider()

    await user.click(screen.getByTestId('start'))
    await waitFor(() => {
      expect(screen.getByText('Gateway started')).toBeTruthy()
    })
  })

  it('still surfaces thrown request failures as error toasts', async () => {
    const user = userEvent.setup()
    mockMyStart.mockRejectedValue(new Error('gateway failed to start'))
    renderProvider()

    await user.click(screen.getByTestId('start'))

    await waitFor(() => {
      expect(screen.getByText('gateway failed to start')).toBeTruthy()
    })
    expect(screen.queryByText('Gateway started')).toBeNull()
  })
})
