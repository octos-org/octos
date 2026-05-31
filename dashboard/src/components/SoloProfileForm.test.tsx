// Tests for SoloProfileForm — the no-password local onboarding form.
// Validation mirrors the server validators; the form must keep submit
// disabled until name/username/email are valid, send trimmed values, and
// surface a server rejection.

import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter } from 'react-router-dom'
import SoloProfileForm from './SoloProfileForm'

const soloCreate = vi.fn()
vi.mock('../contexts/AuthContext', () => ({
  useAuth: () => ({ soloCreate }),
}))

beforeEach(() => {
  soloCreate.mockReset()
})

function renderForm(onDone?: () => void) {
  return render(
    <MemoryRouter>
      <SoloProfileForm onDone={onDone} />
    </MemoryRouter>,
  )
}

describe('SoloProfileForm', () => {
  it('keeps submit disabled until name, username and email are valid', async () => {
    const user = userEvent.setup()
    renderForm()
    const submit = screen.getByTestId('solo-submit')
    expect(submit).toBeDisabled()

    await user.type(screen.getByTestId('solo-name'), 'Ada Lovelace')
    await user.type(screen.getByTestId('solo-username'), 'ada')
    expect(submit).toBeDisabled() // email still missing

    await user.type(screen.getByTestId('solo-email'), 'ada@example.com')
    expect(submit).toBeEnabled()
  })

  it('rejects a username containing spaces', async () => {
    const user = userEvent.setup()
    renderForm()
    await user.type(screen.getByTestId('solo-name'), 'Ada')
    await user.type(screen.getByTestId('solo-username'), 'has space')
    await user.type(screen.getByTestId('solo-email'), 'ada@example.com')
    expect(screen.getByTestId('solo-submit')).toBeDisabled()
  })

  it('rejects an email with no @', async () => {
    const user = userEvent.setup()
    renderForm()
    await user.type(screen.getByTestId('solo-name'), 'Ada')
    await user.type(screen.getByTestId('solo-username'), 'ada')
    await user.type(screen.getByTestId('solo-email'), 'not-an-email')
    expect(screen.getByTestId('solo-submit')).toBeDisabled()
  })

  it('submits trimmed values and calls onDone on success', async () => {
    const user = userEvent.setup()
    soloCreate.mockResolvedValue(undefined)
    const onDone = vi.fn()
    renderForm(onDone)

    await user.type(screen.getByTestId('solo-name'), '  Ada Lovelace ')
    await user.type(screen.getByTestId('solo-username'), ' ada ')
    await user.type(screen.getByTestId('solo-email'), ' ada@example.com ')
    await user.click(screen.getByTestId('solo-submit'))

    await waitFor(() =>
      expect(soloCreate).toHaveBeenCalledWith({
        name: 'Ada Lovelace',
        username: 'ada',
        email: 'ada@example.com',
      }),
    )
    await waitFor(() => expect(onDone).toHaveBeenCalled())
  })

  it('surfaces a server rejection', async () => {
    const user = userEvent.setup()
    soloCreate.mockRejectedValue(new Error('username taken'))
    renderForm()

    await user.type(screen.getByTestId('solo-name'), 'Ada')
    await user.type(screen.getByTestId('solo-username'), 'ada')
    await user.type(screen.getByTestId('solo-email'), 'ada@example.com')
    await user.click(screen.getByTestId('solo-submit'))

    await waitFor(() =>
      expect(screen.getByTestId('solo-error')).toHaveTextContent('username taken'),
    )
  })
})
