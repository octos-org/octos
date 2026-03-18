import { useState, useEffect, useRef } from 'react'

interface Props {
  profileId: string
}

export default function LogViewer({ profileId }: Props) {
  const [lines, setLines] = useState<string[]>([])
  const [connected, setConnected] = useState(false)
  const containerRef = useRef<HTMLDivElement>(null)
  const autoScrollRef = useRef(true)

  useEffect(() => {
    setLines([])
    setConnected(false)

    const token = localStorage.getItem('octos_auth_token')
    const base = `/api/admin/profiles/${encodeURIComponent(profileId)}/logs`
    const url = token ? `${base}?token=${encodeURIComponent(token)}` : base
    const eventSource = new EventSource(url)

    eventSource.onopen = () => setConnected(true)

    eventSource.onmessage = (event) => {
      setLines((prev) => {
        const next = [...prev, event.data]
        // Keep last 1000 lines
        return next.length > 1000 ? next.slice(-1000) : next
      })
    }

    eventSource.onerror = () => {
      setConnected(false)
      eventSource.close()
    }

    return () => {
      eventSource.close()
    }
  }, [profileId])

  // Auto-scroll
  useEffect(() => {
    if (autoScrollRef.current && containerRef.current) {
      containerRef.current.scrollTop = containerRef.current.scrollHeight
    }
  }, [lines])

  const handleScroll = () => {
    if (!containerRef.current) return
    const { scrollTop, scrollHeight, clientHeight } = containerRef.current
    autoScrollRef.current = scrollHeight - scrollTop - clientHeight < 40
  }

  return (
    <div className="bg-surface rounded-xl border border-gray-700/50 overflow-hidden">
      <div className="flex items-center justify-between px-4 py-2 border-b border-gray-700/50">
        <h3 className="text-sm font-semibold text-white">Logs</h3>
        <div className="flex items-center gap-2">
          <span
            className={`w-2 h-2 rounded-full ${
              connected ? 'bg-green-400' : 'bg-gray-500'
            }`}
          />
          <span className="text-xs text-gray-500">
            {connected ? 'Connected' : 'Disconnected'}
          </span>
          <button
            onClick={() => setLines([])}
            className="text-xs text-gray-500 hover:text-gray-300 ml-2"
          >
            Clear
          </button>
        </div>
      </div>
      <div
        ref={containerRef}
        onScroll={handleScroll}
        className="log-viewer h-80 overflow-y-auto p-4 bg-surface-dark"
      >
        {lines.length === 0 ? (
          <div className="text-gray-600 text-center py-8">
            {connected ? 'Waiting for output...' : 'No logs available. Start the gateway to see logs.'}
          </div>
        ) : (
          lines.map((line, i) => (
            <div key={i} className="text-gray-300 whitespace-pre-wrap break-all">
              {line.startsWith('[stderr]') ? (
                <span className="text-yellow-400">{line}</span>
              ) : (
                line
              )}
            </div>
          ))
        )}
      </div>
    </div>
  )
}
