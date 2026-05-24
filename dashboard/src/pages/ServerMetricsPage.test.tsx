import { act, render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
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
    total_bytes: 16 * 1024 * 1024,
    used_bytes: 8 * 1024 * 1024,
    available_bytes: 8 * 1024 * 1024,
  },
  swap: {
    total_bytes: 0,
    used_bytes: 0,
  },
  disks: [],
  top_processes: [],
  platform: {
    hostname: 'test-host',
    os: 'TestOS',
    os_version: '1.0',
    uptime_secs: 3600,
  },
}

async function flushFetch() {
  await act(async () => {
    await Promise.resolve()
    await Promise.resolve()
  })
}

describe('ServerMetricsPage freshness indicator', () => {
  beforeEach(() => {
    vi.useFakeTimers({ now: new Date('2026-05-24T00:00:00Z') })
    mockSystemMetrics.mockReset()
  })

  afterEach(() => {
    vi.useRealTimers()
  })

  it('shows how long ago metrics were updated', async () => {
    mockSystemMetrics.mockResolvedValue(metrics)

    render(<ServerMetricsPage />)

    await flushFetch()
    expect(screen.getByText('test-host')).toBeInTheDocument()
    expect(screen.getByText('Updated 0s ago')).toBeInTheDocument()

    act(() => {
      vi.advanceTimersByTime(2000)
    })

    expect(screen.getByText('Updated 2s ago')).toBeInTheDocument()
  })

  it('shows the last successful update age when refresh fails', async () => {
    mockSystemMetrics
      .mockResolvedValueOnce(metrics)
      .mockRejectedValueOnce(new Error('network down'))

    render(<ServerMetricsPage />)

    await flushFetch()
    expect(screen.getByText('test-host')).toBeInTheDocument()

    await act(async () => {
      vi.advanceTimersByTime(5000)
      await Promise.resolve()
    })

    expect(screen.getByText(/Update failed: network down/)).toBeInTheDocument()
    expect(screen.getByText(/Last successful update: 5s ago/)).toBeInTheDocument()
  })
})
