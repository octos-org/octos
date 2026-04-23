type Props = {
  step: number
  totalSteps: number
  onBack: () => void
  onNext: () => void
  onSkipStep: () => void
  onSkipWizard: () => void
  onFinish: () => void
  /** When true, the step renders its own finish/next CTAs; nav shows only Back + Skip wizard. */
  stepOwnsPrimary?: boolean
}

export default function WizardNav({
  step,
  totalSteps,
  onBack,
  onNext,
  onSkipStep,
  onSkipWizard,
  onFinish,
  stepOwnsPrimary = false,
}: Props) {
  const isFirst = step <= 0
  const isLast = step >= totalSteps - 1

  return (
    <div className="flex items-center justify-between border-t border-gray-700/50 pt-4 mt-6">
      <div className="flex gap-2">
        <button
          type="button"
          onClick={onBack}
          disabled={isFirst}
          className="px-3 py-2 text-sm font-medium bg-white/5 hover:bg-white/10 text-gray-200 rounded-lg transition disabled:opacity-40 disabled:cursor-not-allowed"
        >
          Back
        </button>
        {!stepOwnsPrimary && (
          <button
            type="button"
            onClick={onSkipStep}
            disabled={isLast}
            className="px-3 py-2 text-sm font-medium text-gray-400 hover:text-gray-200 rounded-lg transition disabled:opacity-40 disabled:cursor-not-allowed"
          >
            Skip This Step
          </button>
        )}
      </div>

      <div className="flex gap-2">
        {!stepOwnsPrimary && (
          isLast ? (
            <button
              type="button"
              onClick={onFinish}
              className="px-4 py-2 text-sm font-medium bg-accent hover:bg-accent/90 text-white rounded-lg transition"
            >
              Finish
            </button>
          ) : (
            <button
              type="button"
              onClick={onNext}
              className="px-4 py-2 text-sm font-medium bg-accent hover:bg-accent/90 text-white rounded-lg transition"
            >
              Next
            </button>
          )
        )}
      </div>
    </div>
  )
}
