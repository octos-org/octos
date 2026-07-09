import type { ProcessStatus } from '../types'

interface Props {
  running: boolean
  status?: ProcessStatus['status']
  error?: string | null
  className?: string
}

export default function StatusBadge({ running, status, error, className = '' }: Props) {
  const state = status ?? (running ? 'running' : 'stopped')
  const isConfigurationError = state === 'configuration_error'
  const dotClass = isConfigurationError
    ? 'bg-amber-400 shadow-[0_0_6px_rgba(251,191,36,0.6)]'
    : running
      ? 'bg-green-400 shadow-[0_0_6px_rgba(74,222,128,0.6)]'
      : 'bg-gray-500'
  const textClass = isConfigurationError ? 'text-amber-400' : running ? 'text-green-400' : 'text-gray-500'
  const label = isConfigurationError ? 'Config error' : running ? 'Running' : 'Stopped'

  return (
    <span
      className={`inline-flex items-center gap-1.5 text-xs font-medium ${className}`}
      title={error || label}
    >
      <span
        className={`w-2 h-2 rounded-full ${dotClass}`}
      />
      <span className={textClass}>{label}</span>
    </span>
  )
}
