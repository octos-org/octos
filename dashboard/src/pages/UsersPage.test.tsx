import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import UsersPage from './UsersPage'

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
