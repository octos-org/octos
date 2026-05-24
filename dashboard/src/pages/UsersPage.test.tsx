import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import UsersPage from './UsersPage'
import type { AllowlistEntry, User } from '../types'

const mockListAllowedEmails = vi.fn()
const mockListUsers = vi.fn()
const mockAddAllowedEmail = vi.fn()
const mockDeleteAllowedEmail = vi.fn()
const mockDeleteUser = vi.fn()

vi.mock('../api', () => ({
  api: {
    listAllowedEmails: () => mockListAllowedEmails(),
    listUsers: () => mockListUsers(),
    addAllowedEmail: (data: { email: string; note?: string }) => mockAddAllowedEmail(data),
    deleteAllowedEmail: (email: string) => mockDeleteAllowedEmail(email),
    deleteUser: (id: string) => mockDeleteUser(id),
  },
}))

const allowlisted: AllowlistEntry[] = [
  {
    email: 'registered@example.com',
    note: 'pilot',
    created_at: '2026-05-01T00:00:00Z',
    registered: true,
    registered_user_id: 'user-1',
    registered_name: 'Registered User',
    last_login_at: '2026-05-02T00:00:00Z',
  },
]

const users: User[] = [
  {
    id: 'user-1',
    email: 'registered@example.com',
    name: 'Registered User',
    role: 'user',
    created_at: '2026-05-01T00:00:00Z',
    last_login_at: '2026-05-02T00:00:00Z',
  },
]

function mockLoadedState() {
  mockListAllowedEmails.mockResolvedValue({ entries: allowlisted })
  mockListUsers.mockResolvedValue({ users })
  mockAddAllowedEmail.mockResolvedValue({})
  mockDeleteAllowedEmail.mockResolvedValue({})
  mockDeleteUser.mockResolvedValue({})
}

describe('UsersPage confirmation dialogs', () => {
  beforeEach(() => {
    vi.restoreAllMocks()
    vi.clearAllMocks()
    mockLoadedState()
  })

  it('removes allowlist entries through ConfirmDialog without native confirm', async () => {
    const confirmSpy = vi.spyOn(window, 'confirm')
    const user = userEvent.setup()

    render(<UsersPage />)

    await user.click(await screen.findByRole('button', { name: 'Remove' }))

    expect(confirmSpy).not.toHaveBeenCalled()
    expect(
      screen.getByText(/future OTP signup will no longer be pre-authorized/i),
    ).toBeInTheDocument()
    expect(mockDeleteAllowedEmail).not.toHaveBeenCalled()

    const removeButtons = screen.getAllByRole('button', { name: 'Remove' })
    await user.click(removeButtons[removeButtons.length - 1])

    await waitFor(() => {
      expect(mockDeleteAllowedEmail).toHaveBeenCalledWith('registered@example.com')
    })
  })

  it('deletes users through ConfirmDialog and respects cancel', async () => {
    const confirmSpy = vi.spyOn(window, 'confirm')
    const user = userEvent.setup()

    render(<UsersPage />)

    await user.click(await screen.findByRole('button', { name: 'Delete' }))

    expect(confirmSpy).not.toHaveBeenCalled()
    expect(
      screen.getByText(/will also delete the profile and stop its gateway/i),
    ).toBeInTheDocument()

    await user.click(screen.getByRole('button', { name: 'Cancel' }))
    expect(mockDeleteUser).not.toHaveBeenCalled()

    await user.click(screen.getByRole('button', { name: 'Delete' }))
    const deleteButtons = screen.getAllByRole('button', { name: 'Delete' })
    await user.click(deleteButtons[deleteButtons.length - 1])

    await waitFor(() => {
      expect(mockDeleteUser).toHaveBeenCalledWith('user-1')
    })
  })
})
