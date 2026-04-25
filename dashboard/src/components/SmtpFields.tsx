import {
  forwardRef,
  useEffect,
  useImperativeHandle,
  useState,
} from 'react'
import { api, type SmtpSettings } from '../api'

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

export type SmtpFieldsHandle = {
  /** Saves the current form values. Resolves true on success, false on failure or when required fields are missing. */
  save: () => Promise<boolean>
}

type Props = {
  /** Optional banner under the heading explaining when SMTP is required. */
  helpText?: string
  /** Called after a successful save (e.g. when the wizard wants to advance). */
  onSaved?: () => void
  /** Whether to render a "Save and continue" button next to "Save". */
  showContinueButton?: boolean
  /** Disable Save+Continue until required fields are present (wizard mode). */
  requireAllFields?: boolean
  /** Continue handler (only used when showContinueButton is true). */
  onContinue?: () => void
  /** When true, suppress the inline Save / Save-and-Continue button row entirely
   * (e.g. wizard mode where the parent owns the Next button via WizardNav). */
  hideButtons?: boolean
  /** Notifies the parent when field validity changes so it can gate its own
   * primary CTA (e.g. WizardNav's Next). Always fired with the current value
   * regardless of `requireAllFields`. */
  onCanProceedChange?: (canProceed: boolean) => void
}

/**
 * Shared SMTP configuration form backed by the dashboard SMTP store
 * (`/api/admin/smtp` + `smtp_secret.json`). Used both by the setup wizard
 * and the per-profile EmailTab so that all SMTP edits target the same
 * single source of truth.
 */
const SmtpFields = forwardRef<SmtpFieldsHandle, Props>(function SmtpFields(
  {
    helpText,
    onSaved,
    showContinueButton = false,
    requireAllFields = false,
    onContinue,
    hideButtons = false,
    onCanProceedChange,
  },
  ref,
) {
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

  const allFieldsFilled =
    host.trim().length > 0 &&
    port > 0 &&
    username.trim().length > 0 &&
    fromAddress.includes('@') &&
    (password.length > 0 || passwordConfigured)

  const canContinue = !requireAllFields || allFieldsFilled

  useEffect(() => {
    onCanProceedChange?.(canContinue)
  }, [canContinue, onCanProceedChange])

  const persist = async () => {
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
  }

  const handleSave = async () => {
    setSave({ kind: 'saving' })
    try {
      await persist()
      setSave({ kind: 'saved' })
      onSaved?.()
    } catch (e: any) {
      setSave({ kind: 'err', error: e?.message || 'Save failed.' })
    }
  }

  const handleSaveAndContinue = async () => {
    if (!canContinue) return
    await handleSave()
    onContinue?.()
  }

  useImperativeHandle(
    ref,
    () => ({
      save: async () => {
        if (!canContinue) return false
        setSave({ kind: 'saving' })
        try {
          await persist()
          setSave({ kind: 'idle' })
          return true
        } catch (e: any) {
          setSave({ kind: 'err', error: e?.message || 'Save failed.' })
          return false
        }
      },
    }),
    [canContinue, host, port, username, fromAddress, password],
  )

  const handleTest = async () => {
    setTest({ kind: 'loading' })
    try {
      // Persist current inputs first so the backend tests what's on screen.
      await persist()
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

  return (
    <div className="space-y-4">
      {helpText && <p className="text-sm text-gray-400">{helpText}</p>}

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
        {test.kind === 'ok' && <div className="text-xs text-green-400">✓ {test.message}</div>}
        {test.kind === 'err' && <div className="text-xs text-red-400 break-all">✗ {test.error}</div>}
      </div>

      {hideButtons ? (
        <>
          {save.kind === 'saving' && <div className="text-xs text-gray-400">Saving…</div>}
          {save.kind === 'err' && (
            <div className="text-xs text-red-400 break-all">{save.error}</div>
          )}
        </>
      ) : (
        <div className="flex items-center gap-3 border-t border-gray-700/50 pt-3">
          <button
            type="button"
            onClick={handleSave}
            disabled={save.kind === 'saving'}
            className="px-3 py-2 text-sm font-medium bg-white/5 hover:bg-white/10 text-gray-200 rounded-lg transition disabled:opacity-40 disabled:cursor-not-allowed"
          >
            {save.kind === 'saving' ? 'Saving…' : 'Save'}
          </button>
          {showContinueButton && (
            <button
              type="button"
              onClick={handleSaveAndContinue}
              disabled={!canContinue || save.kind === 'saving'}
              className="px-4 py-2 text-sm font-medium bg-accent hover:bg-accent/90 text-white rounded-lg transition disabled:opacity-40 disabled:cursor-not-allowed"
            >
              Save and Continue
            </button>
          )}
          {save.kind === 'saved' && <span className="text-xs text-green-400">Saved</span>}
          {save.kind === 'err' && (
            <span className="text-xs text-red-400 break-all">{save.error}</span>
          )}
        </div>
      )}
    </div>
  )
})

export default SmtpFields
