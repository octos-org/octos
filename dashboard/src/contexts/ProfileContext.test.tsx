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
import { MemoryRouter, Route, Routes } from 'react-router-dom'
import { ProfileProvider, useProfile } from './ProfileContext'
import { ToastProvider } from '../components/Toast'

const {
  mockApiStart,
  mockApiStop,
  mockApiRestart,
  mockMyStart,
  mockMyStop,
  mockMyRestart,
  mockMyStartSub,
  mockMyStopSub,
  mockGetProfile,
  mockAuth,
  PROFILE,
} = vi.hoisted(() => ({
  mockApiStart: vi.fn(),
  mockApiStop: vi.fn(),
  mockApiRestart: vi.fn(),
  mockMyStart: vi.fn(),
  mockMyStop: vi.fn(),
  mockMyRestart: vi.fn(),
  mockMyStartSub: vi.fn(),
  mockMyStopSub: vi.fn(),
  mockGetProfile: vi.fn(),
  mockAuth: { value: { isAdmin: false } as { isAdmin: boolean } },
  PROFILE: {
    id: 'prof-1',
    name: 'Prof One',
    email: 'owner@example.com',
    public_subdomain: 'prof-1',
    enabled: true,
    parent_id: null,
    status: null,
    config: {},
  },
}))

vi.mock('../api', async () => {
  const actual = await vi.importActual<typeof import('../api')>('../api')
  return {
    ...actual,
    api: {
      getProfile: (id: string) => mockGetProfile(PROFILE, id),
      startGateway: (id: string) => mockApiStart(id),
      stopGateway: (id: string) => mockApiStop(id),
      restartGateway: (id: string) => mockApiRestart(id),
    },
    myApi: {
      getProfile: () => mockGetProfile(PROFILE),
      getSubAccount: (id: string) => mockGetProfile(PROFILE, id),
      startGateway: () => mockMyStart(),
      stopGateway: () => mockMyStop(),
      restartGateway: () => mockMyRestart(),
      startSubGateway: (id: string) => mockMyStartSub(id),
      stopSubGateway: (id: string) => mockMyStopSub(id),
    },
    getLogStreamUrl: () => '/api/my/profile/logs',
    getAdminLogStreamUrl: (id: string) => `/api/profiles/${id}/logs`,
  }
})

vi.mock('./AuthContext', () => ({
  useAuth: () => ({
    user: { id: 'user-1', name: 'Owner', email: 'owner@example.com' },
    isAdmin: mockAuth.value.isAdmin,
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

// No path renders `/` (no route param → own-profile adapter); a path like
// `/profile/p1` renders the route-param adapters (sub-account or admin).
function renderProvider(path?: string) {
  return render(
    <MemoryRouter initialEntries={[path ?? '/']}>
      <ToastProvider>
        <Routes>
          <Route path="/" element={<ProfileProvider><GatewayButtons /></ProfileProvider>} />
          <Route path="/profile/:id" element={<ProfileProvider><GatewayButtons /></ProfileProvider>} />
        </Routes>
      </ToastProvider>
    </MemoryRouter>,
  )
}

beforeEach(() => {
  for (const fn of [mockApiStart, mockApiStop, mockApiRestart, mockMyStart, mockMyStop, mockMyRestart, mockMyStartSub, mockMyStopSub, mockGetProfile]) {
    fn.mockReset()
  }
  mockMyStart.mockResolvedValue({ ok: true })
  mockMyStop.mockResolvedValue({ ok: true })
  mockMyRestart.mockResolvedValue({ ok: true })
  mockMyStartSub.mockResolvedValue({ ok: true })
  mockMyStopSub.mockResolvedValue({ ok: true })
  mockApiStart.mockResolvedValue({ ok: true })
  mockGetProfile.mockResolvedValue(PROFILE)
  mockAuth.value = { isAdmin: false }
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

  it('falls back to a generic message when a refusal carries no message', async () => {
    const user = userEvent.setup()
    mockMyStart.mockResolvedValue({ ok: false })
    renderProvider()

    await user.click(screen.getByTestId('start'))

    await waitFor(() => {
      expect(screen.getByText('Failed to start gateway')).toBeTruthy()
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

  it('still toasts success and refreshes the profile when the action reports ok: true', async () => {
    const user = userEvent.setup()
    renderProvider()

    // Initial settle: the mount load plus the rerun once the
    // server-confirmed profile id lands.
    await waitFor(() => {
      expect(mockGetProfile).toHaveBeenCalledTimes(2)
    })

    await user.click(screen.getByTestId('start'))
    await waitFor(() => {
      expect(screen.getByText('Gateway started')).toBeTruthy()
    })
    expect(mockGetProfile).toHaveBeenCalledTimes(3)
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

  it('consumes ok: false on the sub-account adapter start too', async () => {
    const user = userEvent.setup()
    mockGetProfile.mockResolvedValue({ ...PROFILE, id: 'sub-1', parent_id: 'prof-1' })
    mockMyStartSub.mockResolvedValue({ ok: false, message: 'gateway start refused for profile \'sub-1\'' })
    renderProvider('/profile/sub-1')

    await user.click(screen.getByTestId('start'))

    await waitFor(() => {
      expect(mockMyStartSub).toHaveBeenCalledWith('sub-1')
      expect(screen.getByText(/gateway start refused for profile 'sub-1'/)).toBeTruthy()
    })
    expect(screen.queryByText('Gateway started')).toBeNull()
  })

  it('fails the sub-account two-step restart when only the start leg is refused', async () => {
    const user = userEvent.setup()
    mockGetProfile.mockResolvedValue({ ...PROFILE, id: 'sub-1', parent_id: 'prof-1' })
    mockMyStopSub.mockResolvedValue({ ok: true, message: "Gateway 'sub-1' stopped" })
    mockMyStartSub.mockResolvedValue({ ok: false, message: 'gateway start refused for profile \'sub-1\'' })
    renderProvider('/profile/sub-1')

    await user.click(screen.getByTestId('restart'))

    await waitFor(() => {
      expect(mockMyStartSub).toHaveBeenCalledWith('sub-1')
      expect(screen.getByText(/gateway start refused for profile 'sub-1'/)).toBeTruthy()
    })
    expect(screen.queryByText('Gateway restarted')).toBeNull()
  })

  it('keeps the admin adapter success path toasting', async () => {
    const user = userEvent.setup()
    mockGetProfile.mockResolvedValue({ ...PROFILE, id: 'admin-1' })
    mockAuth.value = { isAdmin: true }
    renderProvider('/profile/admin-1')

    await user.click(screen.getByTestId('start'))

    await waitFor(() => {
      expect(mockApiStart).toHaveBeenCalledWith('admin-1')
      expect(screen.getByText('Gateway started')).toBeTruthy()
    })
  })
})
