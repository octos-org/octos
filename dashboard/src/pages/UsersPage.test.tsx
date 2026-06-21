import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import UsersPage from './UsersPage'
import type { AllowlistEntry, User } from '../types'

const mockApi = vi.hoisted(() => ({
  listAllowedEmails: vi.fn(),
  listUsers: vi.fn(),
  addAllowedEmail: vi.fn(),
  deleteAllowedEmail: vi.fn(),
  deleteUser: vi.fn(),
}))

const mockToast = vi.hoisted(() => vi.fn())

vi.mock('../api', () => ({ api: mockApi }))
vi.mock('../components/Toast', () => ({
  useToast: () => ({ toast: mockToast }),
}))

const allowedEntry = {
  email: 'alice@example.com',
  note: null,
  created_at: '2026-05-01T00:00:00Z',
  claimed_user_id: null,
  claimed_at: null,
  registered: true,
  registered_user_id: 'alice',
  registered_name: 'Alice',
  last_login_at: null,
}

const account = {
  id: 'alice',
  email: 'alice@example.com',
  name: 'Alice',
  role: 'user',
  created_at: '2026-05-01T00:00:00Z',
  last_login_at: null,
}

beforeEach(() => {
  Object.values(mockApi).forEach((fn) => fn.mockReset())
  mockToast.mockReset()
  mockApi.listAllowedEmails.mockResolvedValue({ entries: [allowedEntry] })
  mockApi.listUsers.mockResolvedValue({ users: [account] })
  mockApi.deleteAllowedEmail.mockResolvedValue({ ok: true })
  mockApi.deleteUser.mockResolvedValue({ ok: true })
})

afterEach(() => {
  vi.restoreAllMocks()
})

describe('UsersPage destructive confirmations', () => {
  it('uses ConfirmDialog instead of native confirm for allowlist removal', async () => {
    const confirmSpy = vi.spyOn(window, 'confirm')
    const user = userEvent.setup()

    render(<UsersPage />)

    await screen.findAllByText('alice@example.com')
    await user.click(screen.getByRole('button', { name: 'Remove' }))

    expect(confirmSpy).not.toHaveBeenCalled()
    expect(screen.getByText('Remove Allowlisted Email')).toBeInTheDocument()
    expect(screen.getByText(/already-registered account will remain/i)).toBeInTheDocument()

    const removeButtons = screen.getAllByRole('button', { name: 'Remove' })
    await user.click(removeButtons[removeButtons.length - 1])

    await waitFor(() => {
      expect(mockApi.deleteAllowedEmail).toHaveBeenCalledWith('alice@example.com')
    })
  })

  it('cancels allowlist removal without calling the API', async () => {
    const user = userEvent.setup()

    render(<UsersPage />)

    await screen.findAllByText('alice@example.com')
    await user.click(screen.getByRole('button', { name: 'Remove' }))

    const dialog = screen.getByRole('dialog', { name: 'Remove Allowlisted Email' })
    await user.click(within(dialog).getByRole('button', { name: 'Cancel' }))

    expect(screen.queryByRole('dialog', { name: 'Remove Allowlisted Email' })).not.toBeInTheDocument()
    expect(mockApi.deleteAllowedEmail).not.toHaveBeenCalled()
  })

  it('uses ConfirmDialog instead of native confirm for account deletion', async () => {
    const confirmSpy = vi.spyOn(window, 'confirm')
    const user = userEvent.setup()

    render(<UsersPage />)

    await screen.findAllByText('alice@example.com')
    await user.click(screen.getByRole('button', { name: 'Delete' }))

    expect(confirmSpy).not.toHaveBeenCalled()
    expect(screen.getByText('Delete Account')).toBeInTheDocument()
    expect(screen.getByText(/delete the profile and stop its gateway/i)).toBeInTheDocument()

    const deleteButtons = screen.getAllByRole('button', { name: 'Delete' })
    await user.click(deleteButtons[deleteButtons.length - 1])

    await waitFor(() => {
      expect(mockApi.deleteUser).toHaveBeenCalledWith('alice')
    })
  })

  it('closes the account deletion dialog with Escape without calling the API', async () => {
    const user = userEvent.setup()

    render(<UsersPage />)

    await screen.findAllByText('alice@example.com')
    await user.click(screen.getByRole('button', { name: 'Delete' }))

    expect(screen.getByRole('dialog', { name: 'Delete Account' })).toBeInTheDocument()

    await user.keyboard('{Escape}')

    expect(screen.queryByRole('dialog', { name: 'Delete Account' })).not.toBeInTheDocument()
    expect(mockApi.deleteUser).not.toHaveBeenCalled()
  })
})

describe('UsersPage bulk account management', () => {
  const bulkUsers: User[] = [
    {
      id: 'admin-1',
      email: 'admin@example.com',
      name: 'Ada Admin',
      role: 'admin',
      created_at: '2026-05-01T00:00:00Z',
      last_login_at: null,
    },
    {
      id: 'user-1',
      email: 'user@example.com',
      name: 'Uma User',
      role: 'user',
      created_at: '2026-05-02T00:00:00Z',
      last_login_at: null,
    },
    {
      id: 'ops-1',
      email: 'ops@example.com',
      name: 'Omar Ops',
      role: 'user',
      created_at: '2026-05-03T00:00:00Z',
      last_login_at: null,
    },
  ]

  const bulkAllowlist: AllowlistEntry[] = [
    {
      email: 'admin@example.com',
      note: null,
      created_at: '2026-05-01T00:00:00Z',
      claimed_user_id: null,
      registered_user_id: 'admin-1',
      registered_name: 'Ada Admin',
      registered: true,
      last_login_at: null,
    },
  ]

  async function waitForUsers() {
    expect(await screen.findByLabelText(/select user admin@example\.com/i)).toBeInTheDocument()
    expect(screen.getByLabelText(/select user user@example\.com/i)).toBeInTheDocument()
    expect(screen.getByLabelText(/select user ops@example\.com/i)).toBeInTheDocument()
  }

  beforeEach(() => {
    mockApi.listAllowedEmails.mockResolvedValue({ entries: bulkAllowlist })
    mockApi.listUsers.mockResolvedValue({ users: bulkUsers })
  })

  it('filters registered accounts by search text and role', async () => {
    const user = userEvent.setup()
    render(<UsersPage />)
    await waitForUsers()

    await user.selectOptions(screen.getByLabelText(/role/i), 'admin')
    expect(screen.getByLabelText(/select user admin@example\.com/i)).toBeInTheDocument()
    expect(screen.queryByLabelText(/select user user@example\.com/i)).not.toBeInTheDocument()
    expect(screen.queryByLabelText(/select user ops@example\.com/i)).not.toBeInTheDocument()

    await user.selectOptions(screen.getByLabelText(/role/i), 'all')
    await user.type(screen.getByLabelText(/search users/i), 'ops')
    expect(screen.getByLabelText(/select user ops@example\.com/i)).toBeInTheDocument()
    expect(screen.queryByLabelText(/select user admin@example\.com/i)).not.toBeInTheDocument()
    expect(screen.queryByLabelText(/select user user@example\.com/i)).not.toBeInTheDocument()
  })

  it('selects all visible accounts and bulk deletes only that filtered set', async () => {
    const user = userEvent.setup()
    mockApi.deleteUser.mockResolvedValue({ ok: true })
    render(<UsersPage />)
    await waitForUsers()

    await user.type(screen.getByLabelText(/search users/i), 'ops')
    await user.click(screen.getByLabelText(/select all visible users/i))
    expect(screen.getByText('1 selected')).toBeInTheDocument()

    await user.click(screen.getByRole('button', { name: /delete selected/i }))
    const dialog = screen.getByText(/delete selected accounts/i).closest('div')
    expect(dialog).not.toBeNull()
    await user.click(within(dialog as HTMLElement).getByRole('button', { name: /^delete$/i }))

    await waitFor(() => expect(mockApi.deleteUser).toHaveBeenCalledTimes(1))
    expect(mockApi.deleteUser).toHaveBeenCalledWith('ops-1')
    expect(mockToast).toHaveBeenCalledWith('Deleted 1 account')
    expect(screen.getByText('0 selected')).toBeInTheDocument()
  })

  it('reports per-account bulk delete failures and keeps failed accounts selected', async () => {
    const user = userEvent.setup()
    mockApi.deleteUser.mockImplementation(async (id: string) => {
      if (id === 'user-1') {
        throw new Error('gateway still stopping')
      }
      return { ok: true }
    })
    render(<UsersPage />)
    await waitForUsers()

    await user.click(screen.getByLabelText(/select user admin@example\.com/i))
    await user.click(screen.getByLabelText(/select user user@example\.com/i))
    await user.click(screen.getByRole('button', { name: /delete selected/i }))
    const dialog = screen.getByText(/delete selected accounts/i).closest('div')
    expect(dialog).not.toBeNull()
    await user.click(within(dialog as HTMLElement).getByRole('button', { name: /^delete$/i }))

    await waitFor(() => expect(mockApi.deleteUser).toHaveBeenCalledTimes(2))
    expect(await screen.findByRole('alert')).toHaveTextContent(
      'user@example.com: gateway still stopping',
    )
    expect(mockToast).toHaveBeenCalledWith('1 deleted, 1 failed', 'error')
    expect(screen.getByLabelText(/select user admin@example\.com/i)).not.toBeChecked()
    expect(screen.getByLabelText(/select user user@example\.com/i)).toBeChecked()
  })
})
