import { act } from 'react'
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { render, screen } from '@testing-library/react'
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
    hostname: 'metrics-host',
    os: 'Darwin',
    os_version: '26.0',
    uptime_secs: 3660,
  },
}

beforeEach(() => {
  vi.useFakeTimers()
  vi.setSystemTime(new Date('2026-05-28T12:00:00Z'))
  mockSystemMetrics.mockReset()
})

afterEach(() => {
  vi.useRealTimers()
})

describe('ServerMetricsPage freshness indicator', () => {
  it('shows how long ago metrics were successfully updated', async () => {
    mockSystemMetrics.mockResolvedValue(metrics)

    render(<ServerMetricsPage />)
    await act(async () => {
      await Promise.resolve()
    })

    expect(screen.getByText('Updated 0s ago')).toBeInTheDocument()

    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000)
    })

    expect(screen.getByText('Updated 3s ago')).toBeInTheDocument()
  })

  it('shows last successful update age after a refresh failure', async () => {
    mockSystemMetrics
      .mockResolvedValueOnce(metrics)
      .mockRejectedValueOnce(new Error('network down'))

    render(<ServerMetricsPage />)
    await act(async () => {
      await Promise.resolve()
    })

    expect(screen.getByText('Updated 0s ago')).toBeInTheDocument()

    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000)
    })

    expect(screen.getByText(/Update failed: network down/)).toBeInTheDocument()
    expect(screen.getByText(/Last successful update: 5s ago/)).toBeInTheDocument()
    expect(screen.getByText('Updated 5s ago')).toBeInTheDocument()
  })
})
