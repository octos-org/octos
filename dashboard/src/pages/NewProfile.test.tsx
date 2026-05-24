import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter, Route, Routes } from 'react-router-dom'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import NewProfile from './NewProfile'

const mockCreateProfile = vi.fn()

vi.mock('../api', () => ({
  api: {
    createProfile: (data: unknown) => mockCreateProfile(data),
  },
}))

function renderNewProfile(path: string) {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <Routes>
        <Route path="/profiles/new" element={<NewProfile />} />
        <Route path="/profile/:id" element={<div>Profile created</div>} />
      </Routes>
    </MemoryRouter>,
  )
}

describe('NewProfile admin mode shortcut', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    mockCreateProfile.mockResolvedValue({})
  })

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
})
