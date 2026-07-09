import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter } from 'react-router-dom'
import AdminBotPage from './AdminBotPage'
import type { ProfileResponse } from '../types'

const mockMonitorStatus = vi.fn()
const mockListProfiles = vi.fn()
const mockToggleWatchdog = vi.fn()
const mockToggleAlerts = vi.fn()
const mockUpdateProfileMonitor = vi.fn()
const mockToast = vi.fn()

vi.mock('../api', () => ({
  api: {
    monitorStatus: () => mockMonitorStatus(),
    listProfiles: () => mockListProfiles(),
    toggleWatchdog: (enabled: boolean) => mockToggleWatchdog(enabled),
    toggleAlerts: (enabled: boolean) => mockToggleAlerts(enabled),
    updateProfileMonitor: (
      id: string,
      data: { watchdog?: string; alerts?: string },
    ) => mockUpdateProfileMonitor(id, data),
  },
}))

vi.mock('../components/Toast', () => ({
  useToast: () => ({ toast: mockToast }),
}))

const alpha = {
  id: 'alpha',
  name: 'Alpha',
  enabled: true,
  watchdog_enabled: true,
  watchdog_override: null,
  alerts_enabled: false,
  alerts_override: false,
}

function profile(
  id: string,
  name: string,
  adminMode: boolean,
  running: boolean,
): ProfileResponse {
  return {
    id,
    name,
    enabled: true,
    data_dir: null,
    parent_id: null,
    public_subdomain: null,
    config: {
      channels: [],
      gateway: {},
      env_vars: {},
      admin_mode: adminMode,
    },
    created_at: '2026-05-01T00:00:00Z',
    updated_at: '2026-05-01T00:00:00Z',
    status: {
      running,
      pid: running ? 123 : null,
      started_at: running ? '2026-05-01T00:00:00Z' : null,
      uptime_secs: running ? 30 : null,
    },
    email: null,
  }
}

function renderPage() {
  return render(
    <MemoryRouter initialEntries={['/admin-bot']}>
      <AdminBotPage />
    </MemoryRouter>,
  )
}

beforeEach(() => {
  mockMonitorStatus.mockReset()
  mockListProfiles.mockReset()
  mockToggleWatchdog.mockReset()
  mockToggleAlerts.mockReset()
  mockUpdateProfileMonitor.mockReset()
  mockToast.mockReset()
  mockListProfiles.mockResolvedValue([])
})

describe('AdminBotPage', () => {
  it('renders profile monitor override rows with inherited effective state', async () => {
    mockMonitorStatus.mockResolvedValue({
      watchdog_enabled: true,
      alerts_enabled: true,
      profiles: [alpha],
    })

    renderPage()

    expect(await screen.findByText('Alpha')).toBeInTheDocument()
    expect(screen.getByText('alpha')).toBeInTheDocument()
    expect(screen.getAllByText('Effective: enabled')).toHaveLength(1)
    expect(screen.getByText('Effective: disabled')).toBeInTheDocument()
  })

  it('persists a per-profile watchdog override without changing the system default', async () => {
    const user = userEvent.setup()
    mockMonitorStatus.mockResolvedValue({
      watchdog_enabled: true,
      alerts_enabled: true,
      profiles: [alpha],
    })
    mockUpdateProfileMonitor.mockResolvedValue({
      ...alpha,
      watchdog_enabled: false,
      watchdog_override: false,
    })

    renderPage()

    const watchdogSelect = (await screen.findAllByRole('combobox'))[0]
    await user.selectOptions(watchdogSelect, 'disabled')

    expect(mockUpdateProfileMonitor).toHaveBeenCalledWith('alpha', {
      watchdog: 'disabled',
    })
    await waitFor(() => {
      expect(screen.getAllByText('Effective: disabled')).toHaveLength(2)
    })
    expect(mockToggleWatchdog).not.toHaveBeenCalled()
  })

  it('lists admin-mode profiles and highlights the running admin bot', async () => {
    mockMonitorStatus.mockResolvedValue({
      watchdog_enabled: true,
      alerts_enabled: false,
      profiles: [],
    })
    mockListProfiles.mockResolvedValue([
      profile('ops-admin', 'Ops Admin', true, true),
      profile('backup-admin', 'Backup Admin', true, false),
      profile('regular-user', 'Regular User', false, true),
    ])

    renderPage()

    expect(await screen.findAllByText('Ops Admin')).toHaveLength(2)
    expect(screen.getByText('Backup Admin')).toBeInTheDocument()
    expect(screen.queryByText('Regular User')).not.toBeInTheDocument()
    expect(screen.getByText('Running')).toBeInTheDocument()
    expect(screen.getByText('Stopped')).toBeInTheDocument()
    expect(screen.getByText('Active admin bot:')).toBeInTheDocument()
    expect(screen.getByRole('link', { name: 'Create admin profile' })).toHaveAttribute(
      'href',
      '/profiles/new?adminMode=true',
    )
  })

  it('shows an empty state when no admin-mode profile exists', async () => {
    mockMonitorStatus.mockResolvedValue({
      watchdog_enabled: true,
      alerts_enabled: false,
      profiles: [],
    })
    mockListProfiles.mockResolvedValue([
      profile('regular-user', 'Regular User', false, true),
    ])

    renderPage()

    expect(await screen.findByText('No admin-mode profiles found.')).toBeInTheDocument()
    expect(screen.getByText('None running')).toBeInTheDocument()
  })
})
