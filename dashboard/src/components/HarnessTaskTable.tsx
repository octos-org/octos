import type {
  HarnessLifecycleState,
  HarnessTaskView,
} from '../api/harness'

export const LIFECYCLE_ORDER: HarnessLifecycleState[] = [
  'failed',
  'verifying',
  'running',
  'queued',
  'ready',
]

const LIFECYCLE_COLORS: Record<HarnessLifecycleState, string> = {
  failed: 'bg-red-500/15 text-red-300 border border-red-500/30',
  verifying: 'bg-yellow-500/15 text-yellow-300 border border-yellow-500/30',
  running: 'bg-blue-500/15 text-blue-300 border border-blue-500/30',
  queued: 'bg-gray-500/15 text-gray-300 border border-gray-500/30',
  ready: 'bg-green-500/15 text-green-300 border border-green-500/30',
}

function formatRelative(iso: string | null | undefined, now: number): string {
  if (!iso) return '—'
  const t = Date.parse(iso)
  if (Number.isNaN(t)) return iso
  const delta = Math.max(0, Math.round((now - t) / 1000))
  if (delta < 60) return `${delta}s ago`
  if (delta < 3600) return `${Math.round(delta / 60)}m ago`
  if (delta < 86400) return `${Math.round(delta / 3600)}h ago`
  return `${Math.round(delta / 86400)}d ago`
}

function lifecyclePill(state: string) {
  const cls =
    (LIFECYCLE_COLORS as Record<string, string>)[state] ||
    'bg-gray-500/10 text-gray-400 border border-gray-500/20'
  return (
    <span
      className={`px-2 py-0.5 rounded-full text-[11px] font-mono uppercase tracking-wider ${cls}`}
      data-testid={`lifecycle-${state}`}
    >
      {state}
    </span>
  )
}

function sessionShort(id: string): string {
  if (id.length <= 28) return id
  return `${id.slice(0, 12)}…${id.slice(-10)}`
}

function childSessionShort(key: string | null | undefined): string {
  if (!key) return '—'
  const parts = key.split('#')
  const suffix = parts[1] || key
  if (suffix.length <= 18) return suffix
  return `${suffix.slice(0, 10)}…`
}

function artifactSummary(task: HarnessTaskView): string {
  if (task.output_files.length === 0) {
    return task.lifecycle_state === 'ready' ? 'none (missing)' : 'none yet'
  }
  if (task.output_files.length === 1) {
    const only = task.output_files[0]
    return only.length > 30 ? `${only.slice(0, 28)}…` : only
  }
  return `${task.output_files.length} files`
}

function failureCause(task: HarnessTaskView): string {
  if (task.error) return task.error
  if (task.child_terminal_state === 'terminal_failed') return 'validator deny / terminal failure'
  if (task.child_terminal_state === 'retryable_failed') return 'retryable failure'
  if (task.child_join_state === 'orphaned') return 'child session orphaned'
  if (task.derived.stale) return 'stale (no runtime updates)'
  if (task.derived.missing_artifact) return 'terminal ready but no artifact'
  return '—'
}

export interface HarnessTaskTableProps {
  tasks: HarnessTaskView[]
  now: number
}

export default function HarnessTaskTable({ tasks, now }: HarnessTaskTableProps) {
  if (tasks.length === 0) {
    return (
      <div className="text-sm text-gray-500 py-8 text-center border border-dashed border-gray-700/50 rounded-xl">
        No harness tasks reported by any running gateway.
      </div>
    )
  }

  return (
    <div
      className="bg-surface border border-gray-700/50 rounded-xl overflow-hidden"
      data-testid="harness-task-table"
    >
      <table className="w-full text-sm">
        <thead>
          <tr className="text-xs text-gray-500 bg-white/[0.02] border-b border-gray-700/30">
            <th className="text-left py-2 px-3 font-medium">State</th>
            <th className="text-left py-2 px-3 font-medium">Tool / Workflow</th>
            <th className="text-left py-2 px-3 font-medium">Phase</th>
            <th className="text-left py-2 px-3 font-medium">Profile · Session</th>
            <th className="text-left py-2 px-3 font-medium">Child</th>
            <th className="text-left py-2 px-3 font-medium">Artifact</th>
            <th className="text-left py-2 px-3 font-medium">Failure Cause</th>
            <th className="text-right py-2 px-3 font-medium">Updated</th>
          </tr>
        </thead>
        <tbody>
          {tasks.map((task, i) => {
            const rowClasses = [
              i % 2 === 0 ? 'bg-white/[0.015]' : '',
              task.derived.stale ? 'ring-1 ring-inset ring-yellow-500/30' : '',
              task.derived.missing_artifact ? 'ring-1 ring-inset ring-orange-500/30' : '',
              task.derived.validator_failed ? 'ring-1 ring-inset ring-red-500/40' : '',
            ].join(' ')
            return (
              <tr
                key={task.task_id}
                data-testid="harness-task-row"
                data-lifecycle={task.lifecycle_state}
                data-stale={task.derived.stale ? 'true' : 'false'}
                data-missing-artifact={task.derived.missing_artifact ? 'true' : 'false'}
                data-validator-failed={task.derived.validator_failed ? 'true' : 'false'}
                className={`border-b border-gray-700/20 last:border-0 ${rowClasses}`}
              >
                <td className="py-2 px-3">
                  <div className="flex items-center gap-1.5">
                    {lifecyclePill(task.lifecycle_state)}
                    {task.derived.stale && (
                      <span
                        data-testid="badge-stale"
                        title="No runtime updates past the stale threshold"
                        className="px-1.5 py-0.5 text-[10px] rounded bg-yellow-500/10 text-yellow-300 border border-yellow-500/30"
                      >
                        stale
                      </span>
                    )}
                    {task.derived.missing_artifact && (
                      <span
                        data-testid="badge-missing-artifact"
                        title="Task reported ready but exposes no output files"
                        className="px-1.5 py-0.5 text-[10px] rounded bg-orange-500/10 text-orange-300 border border-orange-500/30"
                      >
                        no artifact
                      </span>
                    )}
                    {task.derived.validator_failed && (
                      <span
                        data-testid="badge-validator-failed"
                        title="Child terminal state indicates validator failure"
                        className="px-1.5 py-0.5 text-[10px] rounded bg-red-500/10 text-red-300 border border-red-500/30"
                      >
                        validator
                      </span>
                    )}
                    {task.error &&
                      !task.derived.validator_failed &&
                      !task.derived.stale &&
                      !task.derived.missing_artifact && (
                        <span
                          data-testid="badge-loop-error"
                          title={task.error}
                          className="px-1.5 py-0.5 text-[10px] rounded bg-red-500/10 text-red-300 border border-red-500/30"
                        >
                          loop-err
                        </span>
                      )}
                  </div>
                </td>
                <td className="py-2 px-3">
                  <div className="font-mono text-gray-200">{task.tool_name}</div>
                  {task.workflow_kind && (
                    <div className="text-[11px] text-gray-500">{task.workflow_kind}</div>
                  )}
                </td>
                <td className="py-2 px-3 text-gray-400 font-mono text-[12px]">
                  {task.current_phase || '—'}
                  {task.runtime_state && (
                    <div className="text-[10px] text-gray-600">{task.runtime_state}</div>
                  )}
                </td>
                <td className="py-2 px-3 text-gray-400 font-mono text-[12px]">
                  <div>{task.profile_id}</div>
                  <div className="text-gray-600" title={task.session_id}>
                    {sessionShort(task.session_id)}
                  </div>
                </td>
                <td
                  className="py-2 px-3 text-gray-400 font-mono text-[12px]"
                  title={task.child_session_key || undefined}
                >
                  {childSessionShort(task.child_session_key)}
                  {task.child_join_state && (
                    <div className="text-[10px] text-gray-600">{task.child_join_state}</div>
                  )}
                </td>
                <td className="py-2 px-3 text-gray-400 text-[12px]" title={task.output_files.join('\n')}>
                  {artifactSummary(task)}
                </td>
                <td
                  className="py-2 px-3 text-gray-400 text-[12px] max-w-[260px] truncate"
                  title={failureCause(task)}
                >
                  {failureCause(task)}
                </td>
                <td className="py-2 px-3 text-right text-gray-500 text-[12px] whitespace-nowrap">
                  {formatRelative(task.updated_at, now)}
                </td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </div>
  )
}
