import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { Link, MemoryRouter, Route, Routes, useParams } from 'react-router-dom'
import { ProfileProvider, useProfile } from './ProfileContext'
import { ToastProvider } from '../components/Toast'

const mocks = vi.hoisted(() => ({
  get: vi.fn(), update: vi.fn(), stop: vi.fn(), remove: vi.fn(), purge: vi.fn(),
  start: vi.fn(), restart: vi.fn(), ownGet: vi.fn(),
  auth: { isAdmin: true },
}))

vi.mock('./AuthContext', () => ({
  useAuth: () => ({ user: { id: 'owner' }, isAdmin: mocks.auth.isAdmin, scopedProfile: null }),
}))
vi.mock('../api', async () => ({
  ...await vi.importActual<typeof import('../api')>('../api'),
  api: {
    getProfile: mocks.get, updateProfile: mocks.update, stopGateway: mocks.stop,
    startGateway: mocks.start, restartGateway: mocks.restart,
    deleteProfile: mocks.remove, purgeProfile: mocks.purge,
  },
  myApi: {
    getProfile: mocks.ownGet, getSubAccount: mocks.get, updateSubAccount: mocks.update,
    stopSubGateway: mocks.stop, startSubGateway: mocks.start,
  },
  getLogStreamUrl: () => '/my/logs',
  getAdminLogStreamUrl: (id: string) => `/profiles/${id}/logs`,
}))

function profile(id: string) {
  return { id, name: id, config: { channels: [], env_vars: { OWNER: id } }, status: null,
    email: `${id}@example.com`, enabled: true, parent_id: id === 'child' ? 'parent' : null }
}
function deferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((done) => { resolve = done })
  return { promise, resolve }
}
function Actions() {
  const { id } = useParams()
  const value = useProfile()
  return <>
    <span data-testid="route">{id}</span>
    <span data-testid="target">{value.profileId}</span>
    <span data-testid="name">{value.profileName}</span>
    <span data-testid="loading">{String(value.loading)}</span>
    <span data-testid="config">{value.config.env_vars?.OWNER}</span>
    <Link to="/profile/child">Configure child</Link>
    <button onClick={() => void value.stopGateway()}>Stop</button>
    <button onClick={() => void value.startGateway()}>Start</button>
    <button disabled={value.loading} onClick={() => void value.save()}>Save</button>
    <button onClick={() => void value.deleteProfile()}>Delete</button>
    <button onClick={() => void value.purgeProfile()}>Purge</button>
  </>
}
function mount(path = '/profile/parent') {
  render(<MemoryRouter initialEntries={[path]}><ToastProvider><Routes>
    <Route path="/profile/:id" element={<ProfileProvider><Actions /></ProfileProvider>} />
    <Route path="/my" element={<ProfileProvider><Actions /></ProfileProvider>} />
  </Routes></ToastProvider></MemoryRouter>)
}
async function loaded(id: string) {
  await waitFor(() => {
    expect(screen.getByTestId('name')).toHaveTextContent(id)
    expect(screen.getByTestId('loading')).toHaveTextContent('false')
  })
}
beforeEach(() => {
  vi.resetAllMocks()
  mocks.auth.isAdmin = true
  mocks.get.mockImplementation(async (id: string) => profile(id))
  mocks.ownGet.mockResolvedValue(profile('host-scoped'))
  mocks.update.mockImplementation(async (id: string) => profile(id))
  mocks.stop.mockResolvedValue({ ok: true })
  mocks.start.mockResolvedValue({ ok: true })
  mocks.restart.mockResolvedValue({ ok: true })
  mocks.purge.mockResolvedValue({ bytes_freed: 0 })
})

describe('profile navigation identity', () => {
  for (const isAdmin of [true, false]) {
    it(`targets the child for stop and save after navigation (admin=${isAdmin})`, async () => {
      mocks.auth.isAdmin = isAdmin
      const user = userEvent.setup()
      mount()
      await loaded('parent')
      await user.click(screen.getByRole('link', { name: 'Configure child' }))
      expect(screen.getByTestId('route')).toHaveTextContent('child')
      await user.click(screen.getByRole('button', { name: 'Stop' }))
      expect(mocks.stop).toHaveBeenCalledWith('child')
      await loaded('child')
      await user.click(screen.getByRole('button', { name: 'Save' }))
      expect(mocks.update).toHaveBeenCalledWith('child', expect.objectContaining({ name: 'child' }))
    })
  }

  it('targets the child for delete and purge after navigation', async () => {
    const user = userEvent.setup()
    mount()
    await loaded('parent')
    await user.click(screen.getByRole('link', { name: 'Configure child' }))
    await user.click(screen.getByRole('button', { name: 'Delete' }))
    await user.click(screen.getByRole('button', { name: 'Purge' }))
    expect(mocks.remove).toHaveBeenCalledWith('child')
    expect(mocks.purge).toHaveBeenCalledWith('child')
  })

  it('clears the previous profile while the new request is pending', async () => {
    const child = deferred<ReturnType<typeof profile>>()
    mocks.get.mockImplementation((id: string) => id === 'child' ? child.promise : Promise.resolve(profile(id)))
    const user = userEvent.setup()
    mount()
    await loaded('parent')
    await user.click(screen.getByRole('link', { name: 'Configure child' }))
    expect(screen.getByTestId('target')).toHaveTextContent('child')
    expect(screen.getByTestId('name')).toBeEmptyDOMElement()
    expect(screen.getByTestId('config')).toBeEmptyDOMElement()
    expect(screen.getByTestId('loading')).toHaveTextContent('true')
    await act(async () => child.resolve(profile('child')))
    await loaded('child')
  })

  it('ignores an old GET that resolves after the child loads', async () => {
    const parent = deferred<ReturnType<typeof profile>>()
    mocks.get.mockImplementation((id: string) => id === 'parent' ? parent.promise : Promise.resolve(profile(id)))
    const user = userEvent.setup()
    mount()
    await user.click(screen.getByRole('link', { name: 'Configure child' }))
    await loaded('child')
    await act(async () => parent.resolve(profile('parent')))
    expect(screen.getByTestId('name')).toHaveTextContent('child')
    expect(screen.getByTestId('config')).toHaveTextContent('child')
  })

  it('ignores an old save response after navigation', async () => {
    const save = deferred<ReturnType<typeof profile>>()
    mocks.update.mockReturnValue(save.promise)
    const user = userEvent.setup()
    mount()
    await loaded('parent')
    await user.click(screen.getByRole('button', { name: 'Save' }))
    await user.click(screen.getByRole('link', { name: 'Configure child' }))
    await loaded('child')
    await act(async () => save.resolve(profile('parent')))
    expect(screen.getByTestId('name')).toHaveTextContent('child')
    expect(screen.queryByText('Configuration saved')).toBeNull()
  })

  it('does not refresh the old profile after a pending gateway action completes', async () => {
    const start = deferred<{ ok: boolean }>()
    mocks.start.mockReturnValue(start.promise)
    const user = userEvent.setup()
    mount()
    await loaded('parent')
    await user.click(screen.getByRole('button', { name: 'Start' }))
    await user.click(screen.getByRole('link', { name: 'Configure child' }))
    await loaded('child')
    const calls = mocks.get.mock.calls.length
    await act(async () => start.resolve({ ok: true }))
    expect(mocks.get).toHaveBeenCalledTimes(calls)
    expect(screen.queryByText('Gateway started')).toBeNull()
  })

  it('keeps server-confirmed host scope for the own-profile entry', async () => {
    mount('/my')
    await loaded('host-scoped')
    expect(screen.getByTestId('target')).toHaveTextContent('host-scoped')
  })
})
