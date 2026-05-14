// Tests for the Sidebar admin-global navigation gating.
//
// Option Y (issue #315) — the server's host-authoritative `/api/my/*`
// scope re-routes admin to the tenant profile when the request lands on
// a tenant subdomain. To match that on the frontend, the Sidebar hides
// admin-only global links (`/`, `/users`, `/admin-bot`, etc.) when the
// authenticated user is admin AND `scoped_profile` is set on
// `/api/auth/me`. The profile-area links (`Home`, `LLM`, etc.) remain
// since those go to `/my` which the server now scopes to the tenant.

import { describe, it, expect, vi } from 'vitest'
import { render, screen } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'
import Sidebar from './Sidebar'

type MockUser = { role: 'admin' | 'user'; name?: string; email?: string }
type MockScopedProfile = { id: string; name: string; email_login_enabled: boolean } | null

const mockAuthState: { user: MockUser; isAdmin: boolean; scopedProfile: MockScopedProfile } = {
  user: { role: 'admin', email: 'admin@example.com' },
  isAdmin: true,
  scopedProfile: null,
}

vi.mock('../contexts/AuthContext', () => ({
  useAuth: () => ({
    ...mockAuthState,
    logout: vi.fn(),
  }),
}))

function setAuthState(user: MockUser, scopedProfile: MockScopedProfile) {
  mockAuthState.user = user
  mockAuthState.isAdmin = user.role === 'admin'
  mockAuthState.scopedProfile = scopedProfile
}

function renderSidebar() {
  return render(
    <MemoryRouter initialEntries={['/']}>
      <Sidebar />
    </MemoryRouter>,
  )
}

describe('Sidebar admin-global gating', () => {
  it('shows admin-global links when admin is NOT host-scoped', () => {
    setAuthState({ role: 'admin', email: 'admin@example.com' }, null)
    renderSidebar()
    // Both "All Profiles" buttons render (back link + bottom section)
    // when not in a profile route, but the bottom one is the global
    // admin link. Either way, presence proves the global section is
    // rendered.
    expect(screen.getByText('All Profiles')).toBeInTheDocument()
    expect(screen.getByText('Access')).toBeInTheDocument()
    expect(screen.getByText('Admin Bot')).toBeInTheDocument()
    expect(screen.getByText('Server')).toBeInTheDocument()
    expect(screen.getByText('Setup Wizard')).toBeInTheDocument()
  })

  it('hides admin-global links when admin IS host-scoped to a tenant', () => {
    setAuthState(
      { role: 'admin', email: 'admin@example.com' },
      { id: 'dspfac', name: 'DSPFac', email_login_enabled: true },
    )
    renderSidebar()
    // Global links must be gone.
    expect(screen.queryByText('All Profiles')).not.toBeInTheDocument()
    expect(screen.queryByText('Access')).not.toBeInTheDocument()
    expect(screen.queryByText('Admin Bot')).not.toBeInTheDocument()
    expect(screen.queryByText('Server')).not.toBeInTheDocument()
    expect(screen.queryByText('Setup Wizard')).not.toBeInTheDocument()
    // Profile-area links must still render (admin still has nav to
    // configure the tenant's LLM, messaging, etc.).
    expect(screen.getByText('Home')).toBeInTheDocument()
    expect(screen.getByText('LLM Providers')).toBeInTheDocument()
    // The tenant name is shown under the brand to make scope obvious.
    expect(screen.getByText('DSPFac')).toBeInTheDocument()
  })

  it('non-admin user never sees admin-global links regardless of host scope', () => {
    setAuthState({ role: 'user', email: 'user@example.com' }, null)
    renderSidebar()
    expect(screen.queryByText('All Profiles')).not.toBeInTheDocument()
    expect(screen.queryByText('Access')).not.toBeInTheDocument()
    expect(screen.getByText('Home')).toBeInTheDocument()
  })
})
