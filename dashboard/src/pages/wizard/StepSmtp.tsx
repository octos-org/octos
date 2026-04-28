import { useEffect, useState } from 'react'
import { api, type DeploymentMode, type SmtpSettings } from '../../api'

type TestState =
  | { kind: 'idle' }
  | { kind: 'loading' }
  | { kind: 'ok'; message: string }
  | { kind: 'err'; error: string }

type SaveState =
  | { kind: 'idle' }
  | { kind: 'saving' }
  | { kind: 'saved' }
  | { kind: 'err'; error: string }

type Props = {
  /** Current deployment mode — determines whether SMTP is required. */
  mode: DeploymentMode | null
  /** Called by the parent when the user clicks "Save and continue". */
  onContinue: () => void
}

export default function StepSmtp({ mode, onContinue }: Props) {
  const [host, setHost] = useState('')
  const [port, setPort] = useState(465)
  const [username, setUsername] = useState('')
  const [fromAddress, setFromAddress] = useState('')
  const [password, setPassword] = useState('')
  const [passwordConfigured, setPasswordConfigured] = useState(false)
  const [loaded, setLoaded] = useState(false)

  const [testTo, setTestTo] = useState('')
  const [test, setTest] = useState<TestState>({ kind: 'idle' })
  const [save, setSave] = useState<SaveState>({ kind: 'idle' })

  useEffect(() => {
    let cancelled = false
    api
      .getSmtp()
      .then((s: SmtpSettings) => {
        if (cancelled) return
        setHost(s.host)
        setPort(s.port || 465)
        setUsername(s.username)
        setFromAddress(s.from_address)
        setPasswordConfigured(s.password_configured)
        setLoaded(true)
      })
      .catch((e) => {
        if (cancelled) return
        console.warn('getSmtp failed', e)
        setLoaded(true)
      })
    return () => {
      cancelled = true
    }
  }, [])

  const isRequired = mode === 'tenant' || mode === 'cloud'
  const allFieldsFilled =
    host.trim().length > 0 &&
    port > 0 &&
    username.trim().length > 0 &&
    fromAddress.includes('@') &&
    (password.length > 0 || passwordConfigured)
  const canContinue = !isRequired || allFieldsFilled

  const handleSave = async () => {
    setSave({ kind: 'saving' })
    try {
      await api.saveSmtp({
        host: host.trim(),
        port,
        username: username.trim(),
        from_address: fromAddress.trim(),
        password: password.length > 0 ? password : undefined,
      })
      setSave({ kind: 'saved' })
      if (password.length > 0) {
        setPassword('')
        setPasswordConfigured(true)
      }
    } catch (e: any) {
      setSave({ kind: 'err', error: e?.message || 'Save failed.' })
    }
  }

  const handleTest = async () => {
    setTest({ kind: 'loading' })
    try {
      // Persist current inputs first so the backend tests what's on screen.
      await api.saveSmtp({
        host: host.trim(),
        port,
        username: username.trim(),
        from_address: fromAddress.trim(),
        password: password.length > 0 ? password : undefined,
      })
      if (password.length > 0) {
        setPassword('')
        setPasswordConfigured(true)
      }
      const res = await api.testSmtp(testTo.trim())
      if (res.ok) {
        setTest({ kind: 'ok', message: res.message || 'Test email sent.' })
      } else {
        setTest({ kind: 'err', error: res.error || 'Test failed.' })
      }
    } catch (e: any) {
      setTest({ kind: 'err', error: e?.message || 'Test request failed.' })
    }
  }

  const handleSaveAndContinue = async () => {
    if (!canContinue) return
    await handleSave()
    onContinue()
  }

  return (
    <div className="space-y-4">
      <div>
        <h2 className="text-lg font-semibold text-white mb-1">Email (SMTP)</h2>
        <p className="text-sm text-gray-400">
          {isRequired
            ? 'Required for tenant / cloud deployments — used to deliver OTP login codes.'
            : mode === 'local'
              ? 'Optional for local deployments. Without SMTP, OTP codes are logged to the console.'
              : 'Pick a deployment mode first to know whether this step is required.'}
        </p>
      </div>

      <div className="grid grid-cols-2 gap-3">
        <div className="col-span-2">
          <label className="block text-xs font-medium text-gray-400 mb-1">Host</label>
          <input
            type="text"
            value={host}
            onChange={(e) => setHost(e.target.value)}
            placeholder="smtp.example.com"
            className="w-full px-3 py-2 bg-background border border-gray-700 rounded-lg text-sm text-white font-mono focus:outline-none focus:border-accent"
          />
        </div>
        <div>
          <label className="block text-xs font-medium text-gray-400 mb-1">Port</label>
          <input
            type="number"
            min={1}
            value={port}
            onChange={(e) => setPort(Number(e.target.value) || 0)}
            className="w-full px-3 py-2 bg-background border border-gray-700 rounded-lg text-sm text-white font-mono focus:outline-none focus:border-accent"
          />
        </div>
        <div>
          <label className="block text-xs font-medium text-gray-400 mb-1">From address</label>
          <input
            type="email"
            value={fromAddress}
            onChange={(e) => setFromAddress(e.target.value)}
            placeholder="noreply@example.com"
            className="w-full px-3 py-2 bg-background border border-gray-700 rounded-lg text-sm text-white font-mono focus:outline-none focus:border-accent"
          />
        </div>
        <div className="col-span-2">
          <label className="block text-xs font-medium text-gray-400 mb-1">Username</label>
          <input
            type="text"
            value={username}
            onChange={(e) => setUsername(e.target.value)}
            className="w-full px-3 py-2 bg-background border border-gray-700 rounded-lg text-sm text-white font-mono focus:outline-none focus:border-accent"
          />
        </div>
        <div className="col-span-2">
          <label className="block text-xs font-medium text-gray-400 mb-1">
            Password{' '}
            {passwordConfigured && (
              <span className="text-gray-500 font-normal">(stored — leave blank to keep)</span>
            )}
          </label>
          <input
            type="password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            placeholder={passwordConfigured ? '••••••••' : ''}
            autoComplete="new-password"
            className="w-full px-3 py-2 bg-background border border-gray-700 rounded-lg text-sm text-white font-mono focus:outline-none focus:border-accent"
          />
        </div>
      </div>

      <div className="border-t border-gray-700/50 pt-3 space-y-2">
        <label className="block text-xs font-medium text-gray-400">Send test email to</label>
        <div className="flex gap-2">
          <input
            type="email"
            value={testTo}
            onChange={(e) => setTestTo(e.target.value)}
            placeholder="you@example.com"
            className="flex-1 px-3 py-2 bg-background border border-gray-700 rounded-lg text-sm text-white font-mono focus:outline-none focus:border-accent"
          />
          <button
            type="button"
            onClick={handleTest}
            disabled={!testTo.includes('@') || test.kind === 'loading' || !loaded}
            className="px-3 py-2 text-sm font-medium bg-white/5 hover:bg-white/10 text-gray-200 rounded-lg transition disabled:opacity-40 disabled:cursor-not-allowed"
          >
            {test.kind === 'loading' ? 'Sending…' : 'Send Test'}
          </button>
        </div>
        {test.kind === 'ok' && (
          <div className="text-xs text-green-400">✓ {test.message}</div>
        )}
        {test.kind === 'err' && (
          <div className="text-xs text-red-400 break-all">✗ {test.error}</div>
        )}
      </div>

      <div className="flex items-center gap-3 border-t border-gray-700/50 pt-3">
        <button
          type="button"
          onClick={handleSave}
          disabled={save.kind === 'saving'}
          className="px-3 py-2 text-sm font-medium bg-white/5 hover:bg-white/10 text-gray-200 rounded-lg transition disabled:opacity-40 disabled:cursor-not-allowed"
        >
          {save.kind === 'saving' ? 'Saving…' : 'Save'}
        </button>
        <button
          type="button"
          onClick={handleSaveAndContinue}
          disabled={!canContinue || save.kind === 'saving'}
          className="px-4 py-2 text-sm font-medium bg-accent hover:bg-accent/90 text-white rounded-lg transition disabled:opacity-40 disabled:cursor-not-allowed"
        >
          Save and Continue
        </button>
        {save.kind === 'saved' && <span className="text-xs text-green-400">Saved</span>}
        {save.kind === 'err' && <span className="text-xs text-red-400 break-all">{save.error}</span>}
      </div>

      {mode === null && (
        <p className="text-xs text-gray-500">
          Deployment mode hasn't been chosen yet — SMTP is treated as optional until you pick one.
        </p>
      )}
    </div>
  )
}
