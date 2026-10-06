// Tests for the sub-account gateway actions on the profile Overview page.
//
// The self-service sub-account start/stop routes report failures as
// HTTP 200 + `{ ok: false, message }` (e.g. the solo in-process serve
// guard), so the page must consume the ok flag — a refusal must show the
// backend's message as an error toast, never "Gateway '<id>' started".

import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter } from 'react-router-dom'
import HomePage from './HomePage'
import { ToastProvider } from '../../components/Toast'

const {
  mockMyListSubs,
  mockMyStartSub,
  mockMyStopSub,
  mockMyUsage,
  mockApiStart,
} = vi.hoisted(() => ({
  mockMyListSubs: vi.fn(),
  mockMyStartSub: vi.fn(),
  mockMyStopSub: vi.fn(),
  mockMyUsage: vi.fn(),
  mockApiStart: vi.fn(),
}))

vi.mock('../../api', async () => {
  const actual = await vi.importActual<typeof import('../../api')>('../../api')
  return {
    ...actual,
    api: {
      listSubAccounts: () => [],
      startGateway: (id: string) => mockApiStart(id),
      stopGateway: () => {},
      profileUsage: () => Promise.resolve(null),
    },
    myApi: {
      listSubAccounts: () => mockMyListSubs(),
      startSubGateway: (id: string) => mockMyStartSub(id),
      stopSubGateway: (id: string) => mockMyStopSub(id),
      usage: () => mockMyUsage(),
    },
    systemApi: {
      status: () => Promise.resolve({ base_domain: 'test.example' }),
    },
  }
})

vi.mock('../../contexts/AuthContext', () => ({
  useAuth: () => ({ user: { id: 'user-1' }, isAdmin: false, scopedProfile: null }),
}))

vi.mock('../../contexts/ProfileContext', () => ({
  useProfile: () => ({
    profileId: 'prof-1',
    parentId: null,
    config: {},
    setConfig: () => {},
    status: null,
    isOwn: true,
    loading: false,
    saving: false,
    startGateway: () => {},
    stopGateway: () => {},
    restartGateway: () => {},
    profileName: 'Prof One',
    setProfileName: () => {},
    profileEmail: '',
    setProfileEmail: () => {},
    publicSubdomain: 'prof-1',
    setPublicSubdomain: () => {},
    enabled: true,
    setEnabled: () => {},
    save: () => {},
    deleteProfile: () => {},
    purgeProfile: () => {},
  }),
}))

// The main gateway card would render a second "Start" button; it is
// covered by the ProfileContext tests, so keep the page's sub rows alone.
vi.mock('../../components/GatewayControls', () => ({ default: () => null }))

const SUB = {
  id: 'sub-1',
  name: 'Sub One',
  email: null,
  public_subdomain: 'sub-1',
  enabled: true,
  parent_id: 'prof-1',
  status: { running: false, pid: null },
  config: {},
}

function renderHomePage() {
  return render(
    <MemoryRouter>
      <ToastProvider>
        <HomePage />
      </ToastProvider>
    </MemoryRouter>,
  )
}

beforeEach(() => {
  for (const fn of [mockMyListSubs, mockMyStartSub, mockMyStopSub, mockMyUsage, mockApiStart]) {
    fn.mockReset()
  }
  mockMyListSubs.mockResolvedValue([SUB])
  mockMyUsage.mockResolvedValue(null)
  mockMyStartSub.mockResolvedValue({ ok: true })
  mockMyStopSub.mockResolvedValue({ ok: true })
})

describe('HomePage sub-account gateway actions', () => {
  it('shows a refusal as an error toast instead of "Gateway \'sub-1\' started"', async () => {
    const user = userEvent.setup()
    mockMyStartSub.mockResolvedValue({
      ok: false,
      message: "gateway start refused for profile 'sub-1': restart the serve without --solo",
    })
    renderHomePage()

    await waitFor(() => {
      expect(screen.getByText('Sub One')).toBeTruthy()
    })

    await user.click(screen.getByRole('button', { name: 'Start' }))

    await waitFor(() => {
      expect(screen.getByText(/restart the serve without --solo/)).toBeTruthy()
    })
    expect(screen.queryByText("Gateway 'sub-1' started")).toBeNull()
  })

  it('still toasts success when the sub-account start reports ok: true', async () => {
    const user = userEvent.setup()
    renderHomePage()

    await waitFor(() => {
      expect(screen.getByText('Sub One')).toBeTruthy()
    })

    await user.click(screen.getByRole('button', { name: 'Start' }))

    await waitFor(() => {
      expect(screen.getByText("Gateway 'sub-1' started")).toBeTruthy()
    })
  })
})
