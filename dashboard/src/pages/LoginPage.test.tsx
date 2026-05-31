// Tests for the no-password "solo" affordance on the dashboard login page.
//
// The page must:
//   * Offer "Continue without a password" only when /api/auth/status reports
//     local_solo_enabled.
//   * Re-login the existing owner via soloLogin → navigate home.
//   * Drop into the create form when soloLogin 404s (no profile yet).
//   * Never show the affordance on a non-solo host.

import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter, Route, Routes } from 'react-router-dom'
import LoginPage from './LoginPage'
import { ApiError } from '../api'

const mockStatus = vi.fn()
const soloLogin = vi.fn()
const soloCreate = vi.fn()

vi.mock('../api', async () => {
  const actual = await vi.importActual<typeof import('../api')>('../api')
  return {
    ...actual,
    authApi: { ...actual.authApi, status: () => mockStatus() },
  }
})

vi.mock('../contexts/AuthContext', () => ({
  useAuth: () => ({
    user: null,
    sendOtp: vi.fn(),
    verifyOtp: vi.fn(),
    loginWithToken: vi.fn(),
    soloLogin,
    soloCreate,
  }),
}))

beforeEach(() => {
  mockStatus.mockReset()
  soloLogin.mockReset()
  soloCreate.mockReset()
})

function renderLogin() {
  return render(
    <MemoryRouter initialEntries={['/login']}>
      <Routes>
        <Route path="/login" element={<LoginPage />} />
        <Route path="/" element={<div data-testid="home">home</div>} />
      </Routes>
    </MemoryRouter>,
  )
}

describe('LoginPage solo affordance', () => {
  it('shows the solo button when local_solo_enabled is true', async () => {
    mockStatus.mockResolvedValue({ local_solo_enabled: true })
    renderLogin()
    await waitFor(() =>
      expect(screen.getByTestId('solo-continue')).toBeInTheDocument(),
    )
  })

  it('hides the solo button when local_solo_enabled is false', async () => {
    mockStatus.mockResolvedValue({ local_solo_enabled: false })
    renderLogin()
    // The email form is always present; wait for it, then assert no solo button.
    await waitFor(() => expect(screen.getByText('Email address')).toBeInTheDocument())
    expect(screen.queryByTestId('solo-continue')).not.toBeInTheDocument()
  })

  it('logs in the existing owner and navigates home', async () => {
    mockStatus.mockResolvedValue({ local_solo_enabled: true })
    soloLogin.mockResolvedValue(undefined)
    const user = userEvent.setup()
    renderLogin()
    await user.click(await screen.findByTestId('solo-continue'))
    await waitFor(() => expect(soloLogin).toHaveBeenCalled())
    await waitFor(() => expect(screen.getByTestId('home')).toBeInTheDocument())
  })

  it('drops into the create form when no profile exists yet (404)', async () => {
    mockStatus.mockResolvedValue({ local_solo_enabled: true })
    soloLogin.mockRejectedValue(new ApiError(404, 'no solo profile'))
    const user = userEvent.setup()
    renderLogin()
    await user.click(await screen.findByTestId('solo-continue'))
    await waitFor(() =>
      expect(screen.getByTestId('solo-profile-form')).toBeInTheDocument(),
    )
  })
})
