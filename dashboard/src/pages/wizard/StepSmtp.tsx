import { type DeploymentMode } from '../../api'
import SmtpFields from '../../components/SmtpFields'

type Props = {
  /** Current deployment mode — determines whether SMTP is required. */
  mode: DeploymentMode | null
  /** Called by the parent when the user clicks "Save and continue". */
  onContinue: () => void
}

export default function StepSmtp({ mode, onContinue }: Props) {
  const isRequired = mode === 'tenant' || mode === 'cloud'
  const helpText = isRequired
    ? 'Required for tenant / cloud deployments — used to deliver OTP login codes.'
    : mode === 'local'
      ? 'Optional for local deployments. Without SMTP, OTP codes are logged to the console.'
      : 'Pick a deployment mode first to know whether this step is required.'

  return (
    <div className="space-y-4">
      <div>
        <h2 className="text-lg font-semibold text-white mb-1">Email (SMTP)</h2>
      </div>

      <SmtpFields
        helpText={helpText}
        showContinueButton
        requireAllFields={isRequired}
        onContinue={onContinue}
      />

      {mode === null && (
        <p className="text-xs text-gray-500">
          Deployment mode hasn't been chosen yet — SMTP is treated as optional until you pick one.
        </p>
      )}
    </div>
  )
}
