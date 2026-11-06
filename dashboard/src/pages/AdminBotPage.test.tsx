import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter } from 'react-router-dom'
import AdminBotPage from './AdminBotPage'
import type { MonitorProfileStatus, ProfileResponse } from '../types'

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
    // The page loads all pages; in tests one mocked page is the whole set.
    listAllProfiles: () => mockListProfiles(),
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

const alpha: MonitorProfileStatus = {
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

  // #1483: an inherited row follows the system default (admin.rs:
  // override.unwrap_or(system)), so toggling the default must move it too.
  describe('inherited rows and the system default', () => {
    const beta: MonitorProfileStatus = {
      id: 'beta',
      name: 'Beta',
      enabled: true,
      watchdog_enabled: true,
      watchdog_override: true,
      alerts_enabled: false,
      alerts_override: null,
    }
    const status = (watchdog: boolean, alerts: boolean, rows: MonitorProfileStatus[] = [alpha, beta]) => ({
      watchdog_enabled: watchdog,
      alerts_enabled: alerts,
      profiles: rows.map((row) => ({
        ...row,
        watchdog_enabled: row.watchdog_override ?? watchdog,
        alerts_enabled: row.alerts_override ?? alerts,
      })),
    })
    const cell = (name: string, column: 'watchdog' | 'alerts') =>
      within(screen.getByText(name).closest('tr')!).getAllByRole('cell')[
        column === 'watchdog' ? 2 : 3
      ]
    const effective = (name: string, column: 'watchdog' | 'alerts') =>
      within(cell(name, column)).getByText(/^Effective:/).textContent
    const select = (name: string, column: 'watchdog' | 'alerts') =>
      within(cell(name, column)).getByRole('combobox')
    const systemToggle = (label: string) =>
      screen.getByText(label).closest('div')!.parentElement!.querySelector('button')!

    it('moves inherited rows with the system default and leaves overrides alone', async () => {
      const user = userEvent.setup()
      mockMonitorStatus
        .mockResolvedValueOnce(status(true, true))
        .mockResolvedValueOnce(status(false, true))
      mockToggleWatchdog.mockResolvedValue({ ok: true, watchdog_enabled: false })

      renderPage()
      await screen.findByText('Alpha')
      expect(effective('Alpha', 'watchdog')).toBe('Effective: enabled')
      expect(effective('Beta', 'watchdog')).toBe('Effective: enabled')

      await user.click(systemToggle('Watchdog enabled'))

      expect(mockToggleWatchdog).toHaveBeenCalledWith(false)
      await waitFor(() => expect(effective('Alpha', 'watchdog')).toBe('Effective: disabled'))
      // Beta overrides the default, so it stays enabled.
      expect(effective('Beta', 'watchdog')).toBe('Effective: enabled')
      // Alerts are a separate default: untouched.
      expect(effective('Alpha', 'alerts')).toBe('Effective: disabled')
      expect(effective('Beta', 'alerts')).toBe('Effective: enabled')
      expect(mockUpdateProfileMonitor).not.toHaveBeenCalled()
    })

    it('follows inherit → override → inherit, then the default, with the server row each time', async () => {
      const user = userEvent.setup()
      mockMonitorStatus
        .mockResolvedValueOnce(status(true, true, [alpha]))
        .mockResolvedValueOnce(status(true, false, [alpha]))
      mockUpdateProfileMonitor
        .mockResolvedValueOnce({ ...alpha, alerts_override: true, alerts_enabled: true })
        .mockResolvedValueOnce({ ...alpha, alerts_override: null, alerts_enabled: true })
      mockToggleAlerts.mockResolvedValue({ ok: true, alerts_enabled: false })

      renderPage()
      await screen.findByText('Alpha')
      // Alpha overrides alerts off; the default is on.
      expect(effective('Alpha', 'alerts')).toBe('Effective: disabled')

      await user.selectOptions(select('Alpha', 'alerts'), 'enabled')
      await waitFor(() => expect(effective('Alpha', 'alerts')).toBe('Effective: enabled'))
      await user.selectOptions(select('Alpha', 'alerts'), 'inherit')
      await waitFor(() => expect(select('Alpha', 'alerts')).toHaveValue('inherit'))
      expect(effective('Alpha', 'alerts')).toBe('Effective: enabled')

      await user.click(systemToggle('Alerts enabled'))
      await waitFor(() => expect(effective('Alpha', 'alerts')).toBe('Effective: disabled'))
      expect(mockUpdateProfileMonitor).toHaveBeenCalledTimes(2)
    })

    it('shows what the server kept when it did not keep the new default', async () => {
      const user = userEvent.setup()
      // A server with no monitor configured answers the toggle but keeps nothing.
      mockMonitorStatus
        .mockResolvedValueOnce(status(false, false))
        .mockResolvedValueOnce(status(false, false))
      mockToggleWatchdog.mockResolvedValue({ ok: true, watchdog_enabled: true })

      renderPage()
      await screen.findByText('Alpha')
      await user.click(systemToggle('Watchdog enabled'))

      await waitFor(() => expect(mockMonitorStatus).toHaveBeenCalledTimes(2))
      await waitFor(() =>
        expect(mockToast).toHaveBeenCalledWith(
          'The server did not keep this change: watchdog is still disabled.',
          'error',
        ),
      )
      expect(effective('Alpha', 'watchdog')).toBe('Effective: disabled')
      expect(systemToggle('Watchdog enabled')).toHaveAttribute('aria-checked', 'false')
    })

    it('keeps the saved state when a save fails, for the row and for the default', async () => {
      const user = userEvent.setup()
      mockMonitorStatus.mockResolvedValue(status(true, true))
      mockUpdateProfileMonitor.mockRejectedValue(new Error('profile store is read-only'))
      mockToggleWatchdog.mockRejectedValue(new Error('forbidden'))

      renderPage()
      await screen.findByText('Alpha')
      await user.selectOptions(select('Alpha', 'watchdog'), 'disabled')
      await waitFor(() =>
        expect(mockToast).toHaveBeenCalledWith('profile store is read-only', 'error'),
      )
      expect(select('Alpha', 'watchdog')).toHaveValue('inherit')
      expect(effective('Alpha', 'watchdog')).toBe('Effective: enabled')
      expect(select('Alpha', 'watchdog')).not.toBeDisabled()

      await user.click(systemToggle('Watchdog enabled'))
      await waitFor(() => expect(mockToast).toHaveBeenCalledWith('forbidden', 'error'))
      expect(effective('Alpha', 'watchdog')).toBe('Effective: enabled')
      expect(systemToggle('Watchdog enabled')).toHaveAttribute('aria-checked', 'true')
      expect(systemToggle('Watchdog enabled')).not.toBeDisabled()
    })

    it('names its switches and sends one toggle while one is in flight', async () => {
      const user = userEvent.setup()
      mockMonitorStatus.mockResolvedValue(status(true, true))
      let finish: (value: { ok: boolean; watchdog_enabled: boolean }) => void = () => {}
      mockToggleWatchdog.mockReturnValue(new Promise((resolve) => (finish = resolve)))

      renderPage()
      const toggle = await screen.findByRole('switch', { name: 'Watchdog enabled' })
      expect(toggle).toHaveAttribute('aria-checked', 'true')
      await user.click(toggle)
      expect(toggle).toBeDisabled()
      await user.click(toggle)
      expect(mockToggleWatchdog).toHaveBeenCalledTimes(1)
      finish({ ok: true, watchdog_enabled: false })
      await waitFor(() => expect(toggle).not.toBeDisabled())
    })

    it('keeps a row saved while the default changed on the current default', async () => {
      const user = userEvent.setup()
      mockMonitorStatus
        .mockResolvedValueOnce(status(true, true))
        .mockResolvedValueOnce(status(false, true))
      let saved: (row: MonitorProfileStatus) => void = () => {}
      mockUpdateProfileMonitor.mockReturnValue(new Promise((resolve) => (saved = resolve)))
      mockToggleWatchdog.mockResolvedValue({ ok: true, watchdog_enabled: false })

      renderPage()
      await screen.findByText('Alpha')
      // Beta goes back to inheriting; its answer was computed before the default changed.
      await user.selectOptions(select('Beta', 'watchdog'), 'inherit')
      await user.click(systemToggle('Watchdog enabled'))
      await waitFor(() => expect(effective('Alpha', 'watchdog')).toBe('Effective: disabled'))
      saved({ ...beta, watchdog_override: null, watchdog_enabled: true })
      await waitFor(() => expect(select('Beta', 'watchdog')).toHaveValue('inherit'))
      expect(effective('Beta', 'watchdog')).toBe('Effective: disabled')
    })
  })
})
