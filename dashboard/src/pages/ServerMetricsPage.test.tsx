import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import ServerMetricsPage from './ServerMetricsPage'
import type { SystemMetrics } from '../types'

const mockSystemMetrics = vi.fn()

vi.mock('../api', () => ({
  api: {
    systemMetrics: (opts?: { procs?: boolean }) => mockSystemMetrics(opts),
  },
}))

const metrics: SystemMetrics = {
  cpu: {
    usage_percent: 12.5,
    core_count: 8,
    brand: 'Test CPU',
  },
  memory: {
    total_bytes: 16 * 1024 * 1024 * 1024,
    used_bytes: 8 * 1024 * 1024 * 1024,
    available_bytes: 8 * 1024 * 1024 * 1024,
  },
  swap: {
    total_bytes: 0,
    used_bytes: 0,
  },
  disks: [],
  top_processes: [],
  platform: {
    hostname: 'test-host',
    os: 'darwin',
    os_version: '15.0',
    uptime_secs: 3600,
  },
}

beforeEach(() => {
  mockSystemMetrics.mockReset()
})

afterEach(() => {
  vi.clearAllMocks()
})

describe('ServerMetricsPage freshness indicator', () => {
  it('shows the last successful update age next to the live toggle', async () => {
    mockSystemMetrics.mockResolvedValue(metrics)

    render(<ServerMetricsPage />)

    await screen.findByText('test-host')

    expect(screen.getByText(/Updated \d+s ago/)).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /Live/ })).toBeInTheDocument()
    expect(mockSystemMetrics).toHaveBeenCalledWith({ procs: false })
  })

  it('keeps stale metrics visible and shows last successful update after a fetch failure', async () => {
    const user = userEvent.setup()
    mockSystemMetrics
      .mockResolvedValueOnce(metrics)
      .mockRejectedValueOnce(new Error('network down'))

    render(<ServerMetricsPage />)

    await screen.findByText('test-host')
    await user.click(screen.getByRole('button', { name: /Top Processes/ }))

    await waitFor(() =>
      expect(screen.getByText(/Update failed: network down/)).toBeInTheDocument(),
    )
    expect(screen.getByText(/Last successful update: \d+s ago/)).toBeInTheDocument()
    expect(screen.getByText('test-host')).toBeInTheDocument()
    expect(mockSystemMetrics).toHaveBeenLastCalledWith({ procs: true })
  })
})
