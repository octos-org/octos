import { describe, it, expect } from 'vitest'
import {
  formatFreshnessLabel,
  formatLastSuccessfulUpdate,
} from './ServerMetricsPage'

describe('ServerMetricsPage freshness labels', () => {
  it('formats the last update age for the live header', () => {
    const updatedAt = 1_000_000

    expect(formatFreshnessLabel(updatedAt, updatedAt)).toBe('Updated 0s ago')
    expect(formatFreshnessLabel(updatedAt, updatedAt + 12_000)).toBe('Updated 12s ago')
    expect(formatFreshnessLabel(updatedAt, updatedAt + 90_000)).toBe('Updated 1m ago')
    expect(formatFreshnessLabel(updatedAt, updatedAt + 7_200_000)).toBe('Updated 2h ago')
  })

  it('formats missing or stale last-success timestamps for warning copy', () => {
    const updatedAt = 1_000_000

    expect(formatFreshnessLabel(null, updatedAt)).toBe('Not updated yet')
    expect(formatLastSuccessfulUpdate(null, updatedAt)).toBe('never')
    expect(formatLastSuccessfulUpdate(updatedAt, updatedAt + 5_000)).toBe('5s ago')
  })
})
