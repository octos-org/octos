import { useEffect, useRef, useState } from 'react'
import { useNavigate, useSearchParams } from 'react-router-dom'
import { api, type DeploymentMode } from '../api'
import WizardNav from '../components/WizardNav'
import StepOverview from './wizard/StepOverview'
import StepLlmProvider from './wizard/StepLlmProvider'
import StepSmtp, { type StepSmtpHandle } from './wizard/StepSmtp'
import StepDeploymentMode from './wizard/StepDeploymentMode'
import StepCreateProfile from './wizard/StepCreateProfile'

const TOTAL_STEPS = 5

export default function SetupWizard() {
  const navigate = useNavigate()
  const [searchParams, setSearchParams] = useSearchParams()
  const rawStep = Number(searchParams.get('step') ?? '0')
  const step =
    Number.isFinite(rawStep) && rawStep >= 0 && rawStep < TOTAL_STEPS ? rawStep : 0

  const [mode, setMode] = useState<DeploymentMode | null>(null)
  const [smtpCanProceed, setSmtpCanProceed] = useState(true)
  const smtpRef = useRef<StepSmtpHandle>(null)

  useEffect(() => {
    // Ensure the URL has an explicit step once we've clamped.
    if (searchParams.get('step') === null) {
      setSearchParams({ step: String(step) }, { replace: true })
    }
  }, [searchParams, setSearchParams, step])

  useEffect(() => {
    // Load the saved deployment mode once so StepSmtp knows whether SMTP is
    // required. StepDeploymentMode will update this as the user clicks.
    let cancelled = false
    api
      .getDeploymentMode()
      .then((m) => {
        if (!cancelled) setMode(m.mode)
      })
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [])

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

  // The terminal step (StepCreateProfile) forks to two destinations, so it
  // renders its own CTAs instead of using WizardNav's Next/Finish.
  const stepOwnsPrimary = step === TOTAL_STEPS - 1

  const handleNext = async () => {
    if (step === 2 && smtpRef.current) {
      const ok = await smtpRef.current.save()
      if (!ok) return
    }
    goToStep(step + 1)
  }

  const content = (() => {
    switch (step) {
      case 0:
        return <StepOverview />
      case 1:
        return <StepLlmProvider />
      case 2:
        return (
          <StepSmtp ref={smtpRef} mode={mode} onCanProceedChange={setSmtpCanProceed} />
        )
      case 3:
        return <StepDeploymentMode onModeSaved={setMode} />
      case 4:
        return <StepCreateProfile />
      default:
        return <StepOverview />
    }
  })()

  return (
    <div className="max-w-3xl mx-auto p-6">
      <div className="bg-surface border border-gray-700/50 rounded-xl p-6">
        <div className="text-xs text-gray-500 mb-2">
          Step {step + 1} of {TOTAL_STEPS}
        </div>
        <div className="min-h-[16rem]">{content}</div>
        <WizardNav
          step={step}
          totalSteps={TOTAL_STEPS}
          onBack={() => goToStep(step - 1)}
          onNext={handleNext}
          onSkipStep={() => goToStep(step + 1)}
          onSkipWizard={handleSkipWizard}
          onFinish={handleFinish}
          stepOwnsPrimary={stepOwnsPrimary}
          nextDisabled={step === 2 && !smtpCanProceed}
        />
      </div>
    </div>
  )
}
