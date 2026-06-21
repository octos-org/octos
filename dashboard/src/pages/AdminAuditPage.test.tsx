import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import AdminAuditPage from './AdminAuditPage'

const mockListAudit = vi.hoisted(() => vi.fn())
const mockToast = vi.hoisted(() => vi.fn())

vi.mock('../api', () => ({
  api: {
    listAudit: mockListAudit,
  },
}))

vi.mock('../components/Toast', () => ({
  useToast: () => ({ toast: mockToast }),
}))

beforeEach(() => {
  mockListAudit.mockReset()
  mockToast.mockReset()
  mockListAudit.mockResolvedValue({
    total: 1,
    limit: 50,
    offset: 0,
    entries: [
      {
        schema_version: 1,
        id: 'entry-1',
        timestamp: '2026-05-24T10:15:00Z',
        actor: 'admin-token',
        action: 'profile.update',
        target_id: 'demo',
        before_summary: { enabled: false },
        after_summary: { enabled: true },
      },
    ],
  })
})

describe('AdminAuditPage', () => {
  it('renders audit entries from the admin audit API', async () => {
    render(<AdminAuditPage />)

    await waitFor(() => expect(mockListAudit).toHaveBeenCalledWith({
      actor: '',
      action: '',
      from: '',
      to: '',
      limit: 50,
      offset: 0,
    }))
    expect(await screen.findByText('admin-token')).toBeInTheDocument()
    expect(screen.getByText('profile.update')).toBeInTheDocument()
    expect(screen.getByText('demo')).toBeInTheDocument()
  })

  it('submits actor action and date filters', async () => {
    const user = userEvent.setup()
    render(<AdminAuditPage />)
    await screen.findByText('profile.update')

    await user.type(screen.getByPlaceholderText('Actor'), 'admin-token')
    await user.type(screen.getByPlaceholderText('Action'), 'allowlist.add')
    await user.type(screen.getByLabelText('From date'), '2026-05-20')
    await user.type(screen.getByLabelText('To date'), '2026-05-24')
    await user.click(screen.getByRole('button', { name: /^apply$/i }))

    await waitFor(() => expect(mockListAudit).toHaveBeenLastCalledWith({
      actor: 'admin-token',
      action: 'allowlist.add',
      from: '2026-05-20',
      to: '2026-05-24',
      limit: 50,
      offset: 0,
    }))
  })
})
