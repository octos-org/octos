import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter } from 'react-router-dom'
import NewProfile from './NewProfile'

const mockCreateProfile = vi.fn()
const mockNavigate = vi.fn()

vi.mock('../api', async () => {
  const actual = await vi.importActual<typeof import('../api')>('../api')
  return {
    ...actual,
    api: {
      createProfile: (data: unknown) => mockCreateProfile(data),
    },
  }
})

vi.mock('react-router-dom', async () => {
  const actual = await vi.importActual<typeof import('react-router-dom')>('react-router-dom')
  return {
    ...actual,
    useNavigate: () => mockNavigate,
  }
})

function renderPage(path: string) {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <NewProfile />
    </MemoryRouter>,
  )
}

beforeEach(() => {
  mockCreateProfile.mockReset()
  mockCreateProfile.mockResolvedValue({})
  mockNavigate.mockReset()
})

describe('NewProfile', () => {
  it('preselects admin mode from the adminMode query parameter and creates an admin profile', async () => {
    const user = userEvent.setup()
    renderPage('/profiles/new?adminMode=true')

    expect(screen.getByLabelText(/admin mode/i)).toBeChecked()
    await user.type(screen.getByLabelText(/profile id/i), 'admin-bot')
    await user.type(screen.getByLabelText(/display name/i), 'Admin Bot')
    await user.click(screen.getByRole('button', { name: /create profile/i }))

    await waitFor(() => expect(mockCreateProfile).toHaveBeenCalled())
    expect(mockCreateProfile).toHaveBeenCalledWith({
      id: 'admin-bot',
      name: 'Admin Bot',
      public_subdomain: null,
      enabled: true,
      config: {
        channels: [],
        gateway: {},
        env_vars: {},
        admin_mode: true,
      },
    })
    expect(mockNavigate).toHaveBeenCalledWith('/profile/admin-bot')
  })

  it('keeps regular profile creation unchanged without the adminMode query parameter', async () => {
    const user = userEvent.setup()
    renderPage('/profiles/new')

    expect(screen.getByLabelText(/admin mode/i)).not.toBeChecked()
    await user.type(screen.getByLabelText(/profile id/i), 'worker-bot')
    await user.type(screen.getByLabelText(/display name/i), 'Worker Bot')
    await user.click(screen.getByRole('button', { name: /create profile/i }))

    await waitFor(() => expect(mockCreateProfile).toHaveBeenCalled())
    expect(mockCreateProfile).toHaveBeenCalledWith({
      id: 'worker-bot',
      name: 'Worker Bot',
      public_subdomain: null,
      enabled: true,
    })
  })
})
