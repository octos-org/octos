import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import AdminBotPage from './AdminBotPage'

const mockMonitorStatus = vi.fn()
const mockToggleWatchdog = vi.fn()
const mockToggleAlerts = vi.fn()
const mockUpdateProfileMonitor = vi.fn()
const mockToast = vi.fn()

vi.mock('../api', () => ({
  api: {
    monitorStatus: () => mockMonitorStatus(),
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

beforeEach(() => {
  mockMonitorStatus.mockReset()
  mockToggleWatchdog.mockReset()
  mockToggleAlerts.mockReset()
  mockUpdateProfileMonitor.mockReset()
  mockToast.mockReset()
})

describe('AdminBotPage', () => {
  it('renders profile monitor override rows with inherited effective state', async () => {
    mockMonitorStatus.mockResolvedValue({
      watchdog_enabled: true,
      alerts_enabled: true,
      profiles: [alpha],
    })

    render(<AdminBotPage />)

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

    render(<AdminBotPage />)

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
})
