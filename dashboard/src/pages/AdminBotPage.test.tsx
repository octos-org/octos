import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'
import AdminBotPage from './AdminBotPage'
import type { ProfileResponse } from '../types'

const mockMonitorStatus = vi.fn()
const mockListProfiles = vi.fn()

vi.mock('../api', async () => {
  const actual = await vi.importActual<typeof import('../api')>('../api')
  return {
    ...actual,
    api: {
      monitorStatus: () => mockMonitorStatus(),
      listProfiles: () => mockListProfiles(),
      toggleWatchdog: vi.fn(),
      toggleAlerts: vi.fn(),
    },
  }
})

function profile(overrides: Partial<ProfileResponse>): ProfileResponse {
  return {
    id: 'profile-1',
    name: 'Profile 1',
    enabled: true,
    data_dir: null,
    public_subdomain: null,
    config: {
      channels: [],
      gateway: {},
      env_vars: {},
      admin_mode: false,
    },
    created_at: '2026-05-24T00:00:00Z',
    updated_at: '2026-05-24T00:00:00Z',
    status: {
      running: false,
      pid: null,
      started_at: null,
      uptime_secs: null,
    },
    ...overrides,
  }
}

function renderPage() {
  return render(
    <MemoryRouter>
      <AdminBotPage />
    </MemoryRouter>,
  )
}

beforeEach(() => {
  mockMonitorStatus.mockReset()
  mockListProfiles.mockReset()
  mockMonitorStatus.mockResolvedValue({ watchdog_enabled: false, alerts_enabled: false })
})

describe('AdminBotPage', () => {
  it('lists admin-mode profiles with status and the active admin inline', async () => {
    mockListProfiles.mockResolvedValue([
      profile({
        id: 'root-admin',
        name: 'Root Admin',
        config: { channels: [], gateway: {}, env_vars: {}, admin_mode: true },
        status: { running: true, pid: 42, started_at: '2026-05-24T01:00:00Z', uptime_secs: 120 },
      }),
      profile({
        id: 'backup-admin',
        name: 'Backup Admin',
        config: { channels: [], gateway: {}, env_vars: {}, admin_mode: true },
      }),
      profile({
        id: 'normal-user',
        name: 'Normal User',
        config: { channels: [], gateway: {}, env_vars: {}, admin_mode: false },
        status: { running: true, pid: 43, started_at: '2026-05-24T01:00:00Z', uptime_secs: 120 },
      }),
    ])

    renderPage()

    await waitFor(() => expect(screen.getByText(/active admin:/i)).toHaveTextContent('Root Admin'))
    expect(screen.getAllByText('Root Admin').length).toBeGreaterThan(0)
    expect(screen.getByText('Backup Admin')).toBeInTheDocument()
    expect(screen.queryByText('Normal User')).not.toBeInTheDocument()
    expect(screen.getByText('Running')).toBeInTheDocument()
    expect(screen.getByText('Stopped')).toBeInTheDocument()
  })

  it('links directly to admin-mode profile creation', async () => {
    mockListProfiles.mockResolvedValue([])

    renderPage()

    const link = await screen.findByRole('link', { name: /create admin profile/i })
    expect(link).toHaveAttribute('href', '/profiles/new?adminMode=true')
    expect(screen.getByText('No admin bot profiles yet.')).toBeInTheDocument()
    expect(screen.getByText(/none running/i)).toBeInTheDocument()
  })
})
