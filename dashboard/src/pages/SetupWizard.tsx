import { useEffect, useState } from 'react'
import { useNavigate, useSearchParams } from 'react-router-dom'
import { api, type DeploymentMode } from '../api'
import WizardNav from '../components/WizardNav'
import StepOverview from './wizard/StepOverview'
import StepLlmProvider from './wizard/StepLlmProvider'
import StepSmtp from './wizard/StepSmtp'
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

  useEffect(() => {
    // Ensure the URL has an explicit step once we've clamped.
    if (searchParams.get('step') === null) {
      setSearchParams({ step: String(step) }, { replace: true })
    }
  }, [searchParams, setSearchParams, step])

  useEffect(() => {
    // Load the saved deployment mode and detection together so StepSmtp's
    // required-field gating sees the right mode from the start. On a first-run
    // host (mode field absent from config.json) where detection disagrees with
    // the implicit "local" default, persist the detected value here — before
    // the user reaches step 2 — so SMTP correctly treats tenant/cloud as
    // required. StepDeploymentMode will not re-save once `explicit` is true.
    let cancelled = false
    Promise.all([api.getDeploymentMode(), api.detectDeploymentMode()])
      .then(([current, detection]) => {
        if (cancelled) return
        const detectedMode = detection.detected
        if (!current.explicit && detectedMode && detectedMode !== current.mode) {
          setMode(detectedMode)
          api.saveDeploymentMode(detectedMode).catch((e) => {
            console.warn('preload saveDeploymentMode failed', e)
          })
        } else {
          setMode(current.mode)
        }
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

  // Step 2 (SMTP) renders its own "Save and continue" primary CTA so it can
  // gate advancement on required fields when mode ∈ { tenant, cloud }.
  const stepOwnsPrimary = step === 2 || step === TOTAL_STEPS - 1

  const content = (() => {
    switch (step) {
      case 0:
        return <StepOverview />
      case 1:
        return <StepLlmProvider />
      case 2:
        return <StepSmtp mode={mode} onContinue={() => goToStep(step + 1)} />
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
          onNext={() => goToStep(step + 1)}
          onSkipStep={() => goToStep(step + 1)}
          onSkipWizard={handleSkipWizard}
          onFinish={handleFinish}
          stepOwnsPrimary={stepOwnsPrimary}
        />
      </div>
    </div>
  )
}
