import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter, Route, Routes } from 'react-router-dom'
import NewProfile from './NewProfile'

const mockCreateProfile = vi.fn()
const mockToast = vi.fn()

vi.mock('../api', () => ({
  api: {
    createProfile: (data: unknown) => mockCreateProfile(data),
  },
}))

vi.mock('../components/Toast', () => ({
  useToast: () => ({ toast: mockToast }),
}))

function renderNewProfile(path: string) {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <Routes>
        <Route path="/profiles/new" element={<NewProfile />} />
        <Route path="/profile/:id" element={<div>Profile created page</div>} />
      </Routes>
    </MemoryRouter>,
  )
}

beforeEach(() => {
  mockCreateProfile.mockReset()
  mockToast.mockReset()
  mockCreateProfile.mockResolvedValue({})
})

describe('NewProfile admin mode shortcut', () => {
  it('preselects admin mode from the query string and submits admin config', async () => {
    const user = userEvent.setup()
    renderNewProfile('/profiles/new?adminMode=true')

    expect(screen.getByRole('checkbox', { name: 'Admin Mode' })).toBeChecked()

    await user.type(screen.getAllByPlaceholderText('alice-bot')[0], 'ops-admin')
    await user.type(screen.getByPlaceholderText("Alice's Bot"), 'Ops Admin')
    await user.click(screen.getByRole('button', { name: 'Create Profile' }))

    await waitFor(() => {
      expect(mockCreateProfile).toHaveBeenCalledWith(
        expect.objectContaining({
          id: 'ops-admin',
          name: 'Ops Admin',
          config: expect.objectContaining({
            admin_mode: true,
            channels: [],
            gateway: {},
            env_vars: {},
          }),
        }),
      )
    })
  })

  it('leaves admin mode off by default and omits config from the request', async () => {
    const user = userEvent.setup()
    renderNewProfile('/profiles/new')

    expect(screen.getByRole('checkbox', { name: 'Admin Mode' })).not.toBeChecked()

    await user.type(screen.getAllByPlaceholderText('alice-bot')[0], 'plain-bot')
    await user.type(screen.getByPlaceholderText("Alice's Bot"), 'Plain Bot')
    await user.click(screen.getByRole('button', { name: 'Create Profile' }))

    await waitFor(() => {
      expect(mockCreateProfile).toHaveBeenCalledWith({
        id: 'plain-bot',
        name: 'Plain Bot',
        public_subdomain: null,
        enabled: true,
      })
    })
  })
})
