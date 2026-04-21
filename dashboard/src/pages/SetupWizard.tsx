import { useEffect } from 'react'
import { useNavigate, useSearchParams } from 'react-router-dom'
import { api } from '../api'
import WizardNav from '../components/WizardNav'
import StepOverview from './wizard/StepOverview'
import StepLlmProvider from './wizard/StepLlmProvider'
import StepCreateProfile from './wizard/StepCreateProfile'

const TOTAL_STEPS = 3

function renderStep(step: number) {
  switch (step) {
    case 0:
      return <StepOverview />
    case 1:
      return <StepLlmProvider />
    case 2:
      return <StepCreateProfile />
    default:
      return <StepOverview />
  }
}

export default function SetupWizard() {
  const navigate = useNavigate()
  const [searchParams, setSearchParams] = useSearchParams()
  const rawStep = Number(searchParams.get('step') ?? '0')
  const step = Number.isFinite(rawStep) && rawStep >= 0 && rawStep < TOTAL_STEPS ? rawStep : 0

  useEffect(() => {
    // Ensure the URL has an explicit step once we've clamped.
    if (searchParams.get('step') === null) {
      setSearchParams({ step: String(step) }, { replace: true })
    }
  }, [searchParams, setSearchParams, step])

  const goToStep = (next: number) => {
    const clamped = Math.max(0, Math.min(TOTAL_STEPS - 1, next))
    setSearchParams({ step: String(clamped) })
    api.postSetupStep(clamped).catch((e) => {
      console.warn('postSetupStep failed', e)
    })
  }

  const handleSkipWizard = async () => {
    try {
      await api.skipSetup()
    } catch (e) {
      console.warn('skipSetup failed', e)
    }
    navigate('/')
  }

  const handleFinish = async () => {
    try {
      await api.completeSetup()
    } catch (e) {
      console.warn('completeSetup failed', e)
    }
    navigate('/')
  }

  return (
    <div className="max-w-3xl mx-auto p-6">
      <div className="bg-surface border border-gray-700/50 rounded-xl p-6">
        <div className="text-xs text-gray-500 mb-2">
          Step {step + 1} of {TOTAL_STEPS}
        </div>
        <div className="min-h-[16rem]">{renderStep(step)}</div>
        <WizardNav
          step={step}
          totalSteps={TOTAL_STEPS}
          onBack={() => goToStep(step - 1)}
          onNext={() => goToStep(step + 1)}
          onSkipStep={() => goToStep(step + 1)}
          onSkipWizard={handleSkipWizard}
          onFinish={handleFinish}
          stepOwnsPrimary={step === TOTAL_STEPS - 1}
        />
      </div>
    </div>
  )
}
