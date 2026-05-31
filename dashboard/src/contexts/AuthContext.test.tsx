// Tests for the no-password solo paths on AuthContext.
//
// soloCreate must establish an authenticated principal even when the
// follow-up /me refresh fails — otherwise AuthGuard would bounce the
// freshly-created operator back to /login with no error.

import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter } from 'react-router-dom'
import { AuthProvider, useAuth } from './AuthContext'

const mockMe = vi.fn()
const mockSoloCreate = vi.fn()

vi.mock('../api', async () => {
  const actual = await vi.importActual<typeof import('../api')>('../api')
  return {
    ...actual,
    authApi: {
      ...actual.authApi,
      me: () => mockMe(),
      soloCreate: (b: unknown) => mockSoloCreate(b),
    },
  }
})

function Consumer() {
  const { user, soloCreate } = useAuth()
  return (
    <div>
      <button
        data-testid="go"
        onClick={() =>
          void soloCreate({ name: 'Ada', username: 'ada', email: 'ada@example.com' })
        }
      >
        go
      </button>
      <span data-testid="uid">{user?.id ?? 'none'}</span>
      <span data-testid="uname">{user?.name ?? ''}</span>
    </div>
  )
}

function renderProvider() {
  return render(
    <MemoryRouter>
      <AuthProvider>
        <Consumer />
      </AuthProvider>
    </MemoryRouter>,
  )
}

const CREATE_RESULT = {
  profile_id: 'ada',
  user_id: 'ada',
  name: 'Ada',
  username: 'ada',
  email: 'ada@example.com',
  created: true,
  runtime_mode: 'solo',
  token: 'tok123',
}

beforeEach(() => {
  mockMe.mockReset()
  mockSoloCreate.mockReset()
  localStorage.clear()
})

describe('AuthContext soloCreate', () => {
  it('sets the user from the create result even when /me fails', async () => {
    mockSoloCreate.mockResolvedValue(CREATE_RESULT)
    mockMe.mockRejectedValue(new Error('me unreachable'))
    const user = userEvent.setup()
    renderProvider()

    await user.click(screen.getByTestId('go'))

    await waitFor(() => expect(screen.getByTestId('uid')).toHaveTextContent('ada'))
    expect(localStorage.getItem('octos_session_token')).toBe('tok123')
  })

  it('refines the user from /me when it succeeds', async () => {
    mockSoloCreate.mockResolvedValue(CREATE_RESULT)
    mockMe.mockResolvedValue({
      user: {
        id: 'ada',
        email: 'ada@example.com',
        name: 'Ada Lovelace',
        role: 'admin',
        created_at: '2026-01-01T00:00:00Z',
        last_login_at: null,
      },
      profile: null,
      scoped_profile: null,
    })
    const user = userEvent.setup()
    renderProvider()

    await user.click(screen.getByTestId('go'))

    await waitFor(() => expect(screen.getByTestId('uname')).toHaveTextContent('Ada Lovelace'))
  })
})
