import { forwardRef, useImperativeHandle, useRef } from 'react'
import { type DeploymentMode } from '../../api'
import SmtpFields, { type SmtpFieldsHandle } from '../../components/SmtpFields'

export type StepSmtpHandle = {
  /** Saves the current form values. Resolves true on success, false on failure or when required fields are missing. */
  save: () => Promise<boolean>
}

type Props = {
  /** Current deployment mode — determines whether SMTP is required. */
  mode: DeploymentMode | null
  /** Notifies the parent when field validity changes so it can gate the Next button. */
  onCanProceedChange: (canProceed: boolean) => void
}

const StepSmtp = forwardRef<StepSmtpHandle, Props>(function StepSmtp(
  { mode, onCanProceedChange },
  ref,
) {
  const fieldsRef = useRef<SmtpFieldsHandle>(null)
  const isRequired = mode === 'tenant' || mode === 'cloud'
  const helpText = isRequired
    ? 'Required for tenant / cloud deployments — used to deliver OTP login codes.'
    : mode === 'local'
      ? 'Optional for local deployments. Without SMTP, OTP codes are logged to the console.'
      : 'Pick a deployment mode first to know whether this step is required.'

  useImperativeHandle(
    ref,
    () => ({
      save: async () => (await fieldsRef.current?.save()) ?? false,
    }),
    [],
  )

  return (
    <div className="space-y-4">
      <div>
        <h2 className="text-lg font-semibold text-white mb-1">Email (SMTP)</h2>
      </div>

      <SmtpFields
        ref={fieldsRef}
        helpText={helpText}
        requireAllFields={isRequired}
        hideButtons
        onCanProceedChange={onCanProceedChange}
      />

      {mode === null && (
        <p className="text-xs text-gray-500">
          Deployment mode hasn't been chosen yet — SMTP is treated as optional until you pick one.
        </p>
      )}
    </div>
  )
})

export default StepSmtp
