import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import UsersPage from './UsersPage'
import type { AllowlistEntry, User } from '../types'

const mockListAllowedEmails = vi.fn()
const mockListUsers = vi.fn()
const mockDeleteAllowedEmail = vi.fn()
const mockDeleteUser = vi.fn()
const mockToast = vi.fn()

vi.mock('../api', () => ({
  api: {
    listAllowedEmails: () => mockListAllowedEmails(),
    listUsers: () => mockListUsers(),
    deleteAllowedEmail: (email: string) => mockDeleteAllowedEmail(email),
    deleteUser: (id: string) => mockDeleteUser(id),
  },
}))

vi.mock('../components/Toast', () => ({
  useToast: () => ({ toast: mockToast }),
}))

const allowlistEntries: AllowlistEntry[] = [
  {
    email: 'allowed@example.com',
    note: 'pilot',
    created_at: '2026-01-01T00:00:00Z',
    registered: true,
    registered_user_id: 'user-1',
    registered_name: 'Allowed User',
    last_login_at: '2026-01-02T00:00:00Z',
  },
]

const users: User[] = [
  {
    id: 'user-1',
    email: 'allowed@example.com',
    name: 'Allowed User',
    role: 'user',
    created_at: '2026-01-01T00:00:00Z',
    last_login_at: '2026-01-02T00:00:00Z',
  },
]

function renderUsersPage() {
  return render(<UsersPage />)
}

beforeEach(() => {
  mockListAllowedEmails.mockResolvedValue({ entries: allowlistEntries })
  mockListUsers.mockResolvedValue({ users })
  mockDeleteAllowedEmail.mockResolvedValue({ ok: true })
  mockDeleteUser.mockResolvedValue({ ok: true })
  mockToast.mockReset()
})

afterEach(() => {
  vi.restoreAllMocks()
  vi.clearAllMocks()
})

describe('UsersPage destructive confirmations', () => {
  it('uses ConfirmDialog when removing an allowlisted email', async () => {
    const nativeConfirm = vi.spyOn(window, 'confirm').mockReturnValue(true)
    const user = userEvent.setup()

    renderUsersPage()

    await screen.findAllByText('allowed@example.com')
    await user.click(screen.getByRole('button', { name: 'Remove' }))

    expect(nativeConfirm).not.toHaveBeenCalled()

    const dialog = screen.getByRole('dialog', {
      name: 'Remove allowlisted email?',
    })
    expect(dialog).toHaveTextContent(
      'future OTP signup will no longer be pre-authorized',
    )

    await user.click(within(dialog).getByRole('button', { name: 'Remove' }))

    await waitFor(() =>
      expect(mockDeleteAllowedEmail).toHaveBeenCalledWith('allowed@example.com'),
    )
  })

  it('uses ConfirmDialog when deleting a registered account', async () => {
    const nativeConfirm = vi.spyOn(window, 'confirm').mockReturnValue(true)
    const user = userEvent.setup()

    renderUsersPage()

    await screen.findAllByText('allowed@example.com')
    await user.click(screen.getByRole('button', { name: 'Delete' }))

    expect(nativeConfirm).not.toHaveBeenCalled()

    const dialog = screen.getByRole('dialog', { name: 'Delete account?' })
    expect(dialog).toHaveTextContent(
      'This will also delete the profile and stop its gateway.',
    )

    await user.click(within(dialog).getByRole('button', { name: 'Delete' }))

    await waitFor(() => expect(mockDeleteUser).toHaveBeenCalledWith('user-1'))
  })
})
