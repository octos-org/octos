import { render, screen } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import AdminBotPage from './AdminBotPage'
import type { ProfileResponse } from '../types'

const mockListProfiles = vi.fn()
const mockMonitorStatus = vi.fn()
const mockToggleWatchdog = vi.fn()
const mockToggleAlerts = vi.fn()

vi.mock('../api', () => ({
  api: {
    listProfiles: () => mockListProfiles(),
    monitorStatus: () => mockMonitorStatus(),
    toggleWatchdog: (enabled: boolean) => mockToggleWatchdog(enabled),
    toggleAlerts: (enabled: boolean) => mockToggleAlerts(enabled),
  },
}))

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

describe('AdminBotPage', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    mockMonitorStatus.mockResolvedValue({
      watchdog_enabled: true,
      alerts_enabled: false,
    })
  })

  it('lists admin-mode profiles and highlights the running admin bot', async () => {
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
    mockListProfiles.mockResolvedValue([
      profile('regular-user', 'Regular User', false, true),
    ])

    renderPage()

    expect(await screen.findByText('No admin-mode profiles found.')).toBeInTheDocument()
    expect(screen.getByText('None running')).toBeInTheDocument()
  })
})
