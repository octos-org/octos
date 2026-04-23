import { useEffect, useMemo, useState } from 'react'
import {
  CONTRACT_TEMPLATES,
  parseContractBody,
  validateDispatchRequest,
  type DispatchRequest,
  type ValidationIssue,
} from '../../api/swarm'

const DRAFT_KEY = 'octos_swarm_contract_draft'

interface Props {
  value: string
  onChange: (body: string) => void
  onDraftSave?: (body: string) => void
}

/**
 * Monaco-style JSON editor for swarm contract specs. Validates against
 * the M7.5 `ContractSpec` / `SwarmTopology` types on every keystroke and
 * surfaces issues inline. Draft state persists in localStorage.
 */
export default function ContractEditor({ value, onChange, onDraftSave }: Props) {
  const [issues, setIssues] = useState<ValidationIssue[]>([])
  const [parseError, setParseError] = useState<string | null>(null)
  const [saved, setSaved] = useState(false)

  useEffect(() => {
    // Live validate.
    const { parsed, error } = parseContractBody(value)
    if (error) {
      setParseError(error)
      setIssues([])
      return
    }
    setParseError(null)
    const found = parsed ? validateDispatchRequest(parsed) : []
    setIssues(found)
  }, [value])

  useEffect(() => {
    // Restore draft on mount if no external value was passed.
    const draft = localStorage.getItem(DRAFT_KEY)
    if (draft && !value) {
      onChange(draft)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const loadTemplate = (key: keyof typeof CONTRACT_TEMPLATES) => {
    const template = CONTRACT_TEMPLATES[key]
    if (template) {
      onChange(JSON.stringify(template, null, 2))
    }
  }

  const handleSaveDraft = () => {
    localStorage.setItem(DRAFT_KEY, value)
    if (onDraftSave) onDraftSave(value)
    setSaved(true)
    window.setTimeout(() => setSaved(false), 1200)
  }

  const isValid = !parseError && issues.length === 0 && value.trim() !== ''

  const lineCount = useMemo(() => value.split('\n').length, [value])

  return (
    <div
      className="flex flex-col gap-3"
      data-testid="swarm-contract-editor"
    >
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex flex-wrap gap-1.5">
          <TemplateButton label="Parallel N" onClick={() => loadTemplate('parallel-n')} />
          <TemplateButton label="Sequential" onClick={() => loadTemplate('sequential')} />
          <TemplateButton label="Pipeline" onClick={() => loadTemplate('pipeline')} />
          <TemplateButton label="Fanout" onClick={() => loadTemplate('fanout')} />
        </div>
        <div className="flex items-center gap-2">
          <span
            className="text-[11px] uppercase tracking-wider text-gray-500"
            data-testid="swarm-editor-linecount"
          >
            {lineCount} lines
          </span>
          <button
            type="button"
            onClick={handleSaveDraft}
            data-testid="swarm-editor-save-draft"
            className="rounded-lg border border-gray-700/60 bg-white/5 px-3 py-1.5 text-xs text-gray-300 hover:bg-white/10"
          >
            {saved ? 'Saved' : 'Save Draft'}
          </button>
        </div>
      </div>
      <textarea
        value={value}
        onChange={(e) => onChange(e.target.value)}
        data-testid="swarm-editor-textarea"
        aria-label="Contract spec JSON editor"
        spellCheck={false}
        className="h-96 w-full resize-y rounded-xl border border-gray-700/50 bg-surface font-mono text-xs text-gray-200 leading-relaxed p-3 focus:border-accent/60 focus:outline-none"
        placeholder="{\n  // Paste or author a DispatchRequest here.\n  // Use the template buttons above to start.\n}"
      />
      {parseError && (
        <div
          data-testid="swarm-editor-parse-error"
          className="rounded-lg border border-red-500/40 bg-red-500/10 px-3 py-2 text-xs text-red-300"
        >
          JSON parse error: {parseError}
        </div>
      )}
      {!parseError && issues.length > 0 && (
        <ul
          data-testid="swarm-editor-issues"
          className="space-y-1 rounded-lg border border-yellow-500/40 bg-yellow-500/10 px-3 py-2 text-xs text-yellow-200"
        >
          {issues.map((issue, idx) => (
            <li key={`${issue.field}-${idx}`}>
              <span className="font-semibold">{issue.field}:</span> {issue.message}
            </li>
          ))}
        </ul>
      )}
      {isValid && (
        <div
          data-testid="swarm-editor-valid"
          className="rounded-lg border border-green-500/40 bg-green-500/10 px-3 py-2 text-xs text-green-300"
        >
          Contract validates against the M7.5 ContractSpec / SwarmTopology schema.
        </div>
      )}
    </div>
  )
}

function TemplateButton({
  label,
  onClick,
}: {
  label: string
  onClick: () => void
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="rounded-lg border border-gray-700/60 bg-white/5 px-2.5 py-1 text-xs text-gray-300 hover:bg-white/10"
      data-testid={`swarm-template-${label.toLowerCase().replace(/\s+/g, '-')}`}
    >
      {label}
    </button>
  )
}

/**
 * Parse the current editor body into a `DispatchRequest`. Returns the
 * parsed request + any validation issues. Used by the Dispatch tab.
 */
export function parseEditorBody(
  body: string,
): {
  parsed?: DispatchRequest
  error?: string
  issues: ValidationIssue[]
} {
  const { parsed, error } = parseContractBody(body)
  if (error) {
    return { error, issues: [] }
  }
  if (!parsed) {
    return { error: 'empty body', issues: [] }
  }
  return { parsed, issues: validateDispatchRequest(parsed) }
}
