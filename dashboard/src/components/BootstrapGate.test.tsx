// Tests for BootstrapGate — the dashboard's first-run redirect that sends
// fresh installs through `/setup/welcome` → `/setup/rotate-token`.
//
// The gate must:
//   * Send admins with no rotated token through the welcome wizard.
//   * Skip the welcome step when the wizard has already been completed but
//     `admin_token.json` is missing (the operator forced a re-rotation).
//   * Not redirect on 401/403 — those are auth errors that `AuthGuard`
//     handles by sending the user to /login.
//   * Stay conservative on 5xx / network errors (treat as not rotated so we
//     don't paper over a misconfigured server).

import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter, Route, Routes } from 'react-router-dom'
import BootstrapGate from './BootstrapGate'
import { ApiError } from '../api'

const mockGetTokenStatus = vi.fn()
const mockGetSetupState = vi.fn()

vi.mock('../api', async () => {
  const actual = await vi.importActual<typeof import('../api')>('../api')
  return {
    ...actual,
    api: {
      getTokenStatus: () => mockGetTokenStatus(),
      getSetupState: () => mockGetSetupState(),
    },
  }
})

let isAdminMock = true
vi.mock('../contexts/AuthContext', () => ({
  useAuth: () => ({ isAdmin: isAdminMock }),
}))

function renderAt(path: string) {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <Routes>
        <Route
          path="*"
          element={
            <BootstrapGate>
              <div data-testid="children">protected-children</div>
            </BootstrapGate>
          }
        />
        <Route path="/setup/welcome" element={<div data-testid="welcome">welcome</div>} />
        <Route
          path="/setup/rotate-token"
          element={<div data-testid="rotate">rotate</div>}
        />
        <Route path="/login" element={<div data-testid="login">login</div>} />
      </Routes>
    </MemoryRouter>,
  )
}

beforeEach(() => {
  isAdminMock = true
  mockGetTokenStatus.mockReset()
  mockGetSetupState.mockReset()
})

afterEach(() => {
  vi.clearAllMocks()
})

describe('BootstrapGate', () => {
  it('renders children when token is already rotated', async () => {
    mockGetTokenStatus.mockResolvedValue({ rotated: true })
    mockGetSetupState.mockResolvedValue({
      wizard_completed_at: '2026-01-01T00:00:00Z',
      wizard_skipped: false,
      wizard_last_step_reached: 4,
    })
    renderAt('/')
    await waitFor(() => expect(screen.getByTestId('children')).toBeInTheDocument())
  })

  it('redirects to /setup/welcome when token not rotated and wizard not completed', async () => {
    mockGetTokenStatus.mockResolvedValue({ rotated: false })
    mockGetSetupState.mockResolvedValue({
      wizard_completed_at: null,
      wizard_skipped: false,
      wizard_last_step_reached: 0,
    })
    renderAt('/')
    await waitFor(() => expect(screen.getByTestId('welcome')).toBeInTheDocument())
    expect(screen.queryByTestId('rotate')).not.toBeInTheDocument()
  })

  it('skips welcome and redirects to /setup/rotate-token when wizard already completed but token missing', async () => {
    // Operator already ran the welcome wizard once; admin_token.json was then
    // deleted (forced re-rotation). The gate must not loop the user back
    // through the welcome screen — they have already seen it.
    mockGetTokenStatus.mockResolvedValue({ rotated: false })
    mockGetSetupState.mockResolvedValue({
      wizard_completed_at: '2026-01-01T00:00:00Z',
      wizard_skipped: false,
      wizard_last_step_reached: 4,
    })
    renderAt('/')
    await waitFor(() => expect(screen.getByTestId('rotate')).toBeInTheDocument())
    expect(screen.queryByTestId('welcome')).not.toBeInTheDocument()
  })

  it('does not redirect to /setup/welcome when token status returns 401', async () => {
    // 401 means the caller is unauthenticated. AuthGuard upstream handles
    // sending them to /login — we must not hijack that with a bogus wizard
    // redirect (otherwise the wizard chrome flashes through unauth users).
    mockGetTokenStatus.mockRejectedValue(new ApiError(401, 'unauthorized'))
    mockGetSetupState.mockRejectedValue(new ApiError(401, 'unauthorized'))
    renderAt('/')
    await waitFor(() => expect(screen.getByTestId('children')).toBeInTheDocument())
    expect(screen.queryByTestId('welcome')).not.toBeInTheDocument()
  })

  it('does not redirect to /setup/welcome when token status returns 403', async () => {
    mockGetTokenStatus.mockRejectedValue(new ApiError(403, 'forbidden'))
    mockGetSetupState.mockRejectedValue(new ApiError(403, 'forbidden'))
    renderAt('/')
    await waitFor(() => expect(screen.getByTestId('children')).toBeInTheDocument())
    expect(screen.queryByTestId('welcome')).not.toBeInTheDocument()
  })

  it('treats 500 / network errors as "not rotated" (conservative)', async () => {
    mockGetTokenStatus.mockRejectedValue(new ApiError(500, 'server error'))
    mockGetSetupState.mockRejectedValue(new ApiError(500, 'server error'))
    renderAt('/')
    await waitFor(() => expect(screen.getByTestId('welcome')).toBeInTheDocument())
  })

  it('bypasses the gate for non-admin users', async () => {
    isAdminMock = false
    renderAt('/')
    await waitFor(() => expect(screen.getByTestId('children')).toBeInTheDocument())
    expect(mockGetTokenStatus).not.toHaveBeenCalled()
  })
})
