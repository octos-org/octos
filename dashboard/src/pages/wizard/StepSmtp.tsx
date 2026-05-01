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
  // Default-on so a fresh cloud-host install (or any fresh wizard pass)
  // matches cloud-host-deploy.sh's interactive default. Unchecking this
  // forces a deliberate operator decision rather than silently leaving
  // tenants unable to register.
  const [allowSelfRegistration, setAllowSelfRegistration] = useState(true)
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
        // Always default the checkbox to on when the wizard opens. The
        // install-time coercion (cloud-host-deploy.sh forcing
        // ALLOW_SELF_REGISTRATION=false when SMTP is declined) leaves the
        // saved value at false, but the wizard's intent is to surface this
        // as the operator's first chance to flip it on. Reflecting saved
        // state would defeat that. Operators who deliberately want
        // allowlist-only login uncheck and save here — the next wizard
        // visit will show on again, which is a small wart vs. the install-
        // time-coercion silent-disable problem this is closing.
        setAllowSelfRegistration(true)
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
  // Server-side truth for "is SMTP usable right now". The host/from/etc. can
  // be set without a stored password; without the password OTP delivery
  // fails, so the warning is gated on both pieces being present.
  const smtpConfigured = loaded && host.trim().length > 0 && passwordConfigured

  const handleSave = async () => {
    setSave({ kind: 'saving' })
    try {
      await api.saveSmtp({
        host: host.trim(),
        port,
        username: username.trim(),
        from_address: fromAddress.trim(),
        password: password.length > 0 ? password : undefined,
        allow_self_registration: allowSelfRegistration,
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
        allow_self_registration: allowSelfRegistration,
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
        <h2 className="text-lg font-semibold text-white mb-1">System Email (SMTP)</h2>
        <p className="text-sm text-gray-400">
          {isRequired
            ? 'Required for tenant / cloud deployments — used to deliver OTP login codes.'
            : mode === 'local'
              ? 'Optional for local deployments. Without SMTP, OTP codes are logged to the console.'
              : 'Pick a deployment mode first to know whether this step is required.'}
        </p>
      </div>

      {loaded && !smtpConfigured && (
        <div className="rounded-lg border border-amber-500/40 bg-amber-500/5 p-3 text-xs">
          <div className="text-sm font-medium text-amber-300 mb-1">
            SMTP is not configured
          </div>
          <div className="text-gray-300">
            {isRequired
              ? "Tenants log in via email OTP only — without working SMTP they cannot sign in, and registration/install emails issued by the portal won't be delivered. Strongly recommended to configure here before continuing."
              : 'OTP login codes will be logged to the server console only. The dashboard will work via admin-token login, but email-based login will not deliver to mailboxes.'}
          </div>
          <div className="text-gray-400 mt-2">
            You can skip this step ("Skip This Step" below) and revisit any time from the sidebar — but tenant onboarding and email login will not work until it's configured.
          </div>
        </div>
      )}

      {loaded && smtpConfigured && (
        <div className="rounded-lg border border-emerald-500/40 bg-emerald-500/5 p-3 text-xs">
          <div className="text-sm font-medium text-emerald-300 mb-1">
            SMTP is configured
          </div>
          <div className="text-gray-300">
            A host and password are stored. You can adjust the fields below or run "Send Test" to verify delivery before continuing.
          </div>
        </div>
      )}

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

      <label className="flex items-start gap-3 p-3 rounded-lg border border-gray-700/50 bg-background/60 cursor-pointer">
        <input
          type="checkbox"
          checked={allowSelfRegistration}
          onChange={(e) => setAllowSelfRegistration(e.target.checked)}
          className="mt-1"
        />
        <div className="flex-1 text-sm">
          <div className="text-white font-medium">Allow self-registration via email OTP</div>
          <div className="text-xs text-gray-400 mt-0.5">
            New users can sign in with any email by completing the OTP flow — a profile is created on first verify. Leave on for a public cloud relay; turn off if only invited users (allowlist) should be able to log in. Mirrors <span className="font-mono text-gray-300">cloud-host-deploy.sh</span>'s self-registration prompt.
          </div>
        </div>
      </label>

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
