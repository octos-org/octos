// Tests for SetupRotateToken — the dashboard page that replaces the
// bootstrap admin token with a persistent hashed record.
//
// UX moved from "generate a 32-char random secret + email-receipt" to
// "operator types their own password (exactly 8 chars)". The auto-generate /
// copy / email-receipt ceremony is gone — operators chose their secret, so
// they don't need help remembering it. The "exactly 8" constraint (rather
// than "8 or more") makes the requirement legible in the help text without
// nudging operators toward unbounded length they're unlikely to remember.

import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter } from 'react-router-dom'
import SetupRotateToken from './SetupRotateToken'

const mockRotateToken = vi.fn()

vi.mock('../api', async () => {
  const actual = await vi.importActual<typeof import('../api')>('../api')
  return {
    ...actual,
    api: {
      rotateToken: (t: string) => mockRotateToken(t),
    },
  }
})

const mockSwapToken = vi.fn()
const mockNavigate = vi.fn()

vi.mock('../contexts/AuthContext', () => ({
  useAuth: () => ({ swapToken: mockSwapToken }),
}))

vi.mock('react-router-dom', async () => {
  const actual = await vi.importActual<typeof import('react-router-dom')>('react-router-dom')
  return {
    ...actual,
    useNavigate: () => mockNavigate,
  }
})

function renderPage() {
  return render(
    <MemoryRouter>
      <SetupRotateToken />
    </MemoryRouter>,
  )
}

beforeEach(() => {
  mockRotateToken.mockReset()
  mockSwapToken.mockReset()
  mockNavigate.mockReset()
})

describe('SetupRotateToken', () => {
  it('renders a password input (typed by operator) and no generate/copy buttons', () => {
    renderPage()
    expect(screen.getByLabelText(/new admin token/i)).toHaveAttribute('type', 'password')
    expect(screen.queryByRole('button', { name: /generate/i })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: /^copy$/i })).not.toBeInTheDocument()
  })

  it('keeps the submit button disabled until the operator types exactly 8 characters', async () => {
    const user = userEvent.setup()
    renderPage()
    const submit = screen.getByRole('button', { name: /submit/i })
    expect(submit).toBeDisabled()

    const input = screen.getByLabelText(/new admin token/i)
    await user.type(input, '1234567') // 7 chars: still too short
    expect(submit).toBeDisabled()
    expect(
      screen.getByText(/token must be exactly 8 characters/i),
    ).toBeInTheDocument()

    await user.type(input, '8') // total = 8 chars
    expect(submit).toBeEnabled()

    // Typing a 9th character must disable submit again — the constraint is
    // exact, not a lower bound.
    await user.type(input, '9')
    expect(submit).toBeDisabled()
    expect(
      screen.getByText(/token must be exactly 8 characters/i),
    ).toBeInTheDocument()
  })

  it('toggles between password and text input when the show/hide button is clicked', async () => {
    const user = userEvent.setup()
    renderPage()
    const input = screen.getByLabelText(/new admin token/i)
    expect(input).toHaveAttribute('type', 'password')
    await user.click(screen.getByRole('button', { name: /^show$/i }))
    expect(input).toHaveAttribute('type', 'text')
    await user.click(screen.getByRole('button', { name: /^hide$/i }))
    expect(input).toHaveAttribute('type', 'password')
  })

  it('does not allow submitting a 7-character token', async () => {
    const user = userEvent.setup()
    renderPage()
    const input = screen.getByLabelText(/new admin token/i)
    await user.type(input, '1234567')

    const submit = screen.getByRole('button', { name: /submit/i })
    expect(submit).toBeDisabled()
    expect(mockRotateToken).not.toHaveBeenCalled()
  })

  it('submits a valid 8-character token and swaps the auth token on success', async () => {
    mockRotateToken.mockResolvedValue(undefined)
    const user = userEvent.setup()
    renderPage()
    const input = screen.getByLabelText(/new admin token/i)
    await user.type(input, 'pass1234')
    await user.click(screen.getByRole('button', { name: /submit/i }))

    await waitFor(() => expect(mockRotateToken).toHaveBeenCalledWith('pass1234'))
    await waitFor(() => expect(mockSwapToken).toHaveBeenCalledWith('pass1234'))
  })

  it('shows the API error message when rotation fails', async () => {
    mockRotateToken.mockRejectedValue(new Error('server kaboom'))
    const user = userEvent.setup()
    renderPage()
    const input = screen.getByLabelText(/new admin token/i)
    await user.type(input, 'pass1234')
    await user.click(screen.getByRole('button', { name: /submit/i }))
    await waitFor(() => expect(screen.getByText(/server kaboom/i)).toBeInTheDocument())
  })

  it('does not render the email-receipt section', async () => {
    mockRotateToken.mockResolvedValue(undefined)
    const user = userEvent.setup()
    renderPage()
    const input = screen.getByLabelText(/new admin token/i)
    await user.type(input, 'pass1234')
    await user.click(screen.getByRole('button', { name: /submit/i }))
    await waitFor(() => expect(mockRotateToken).toHaveBeenCalled())
    expect(
      screen.queryByPlaceholderText(/you@example\.com/i),
    ).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: /^send$/i })).not.toBeInTheDocument()
  })

  it('trims surrounding whitespace before submitting (login form trims too)', async () => {
    // Regression: persisting " password " would lock the operator out
    // because LoginPage calls `loginWithToken(adminToken.trim())` on next
    // sign-in. Trim on submit so the two surfaces agree.
    mockRotateToken.mockResolvedValue(undefined)
    const user = userEvent.setup()
    renderPage()
    const input = screen.getByLabelText(/new admin token/i)
    await user.type(input, '  pass1234  ')
    await user.click(screen.getByRole('button', { name: /submit/i }))
    await waitFor(() => expect(mockRotateToken).toHaveBeenCalledWith('pass1234'))
    await waitFor(() => expect(mockSwapToken).toHaveBeenCalledWith('pass1234'))
  })

  it('disables submit when the entry is only whitespace', async () => {
    const user = userEvent.setup()
    renderPage()
    const input = screen.getByLabelText(/new admin token/i)
    await user.type(input, '          ') // 10 spaces — trims to empty
    expect(screen.getByRole('button', { name: /submit/i })).toBeDisabled()
    expect(mockRotateToken).not.toHaveBeenCalled()
  })
})
