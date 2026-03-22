import { useState, useEffect, useRef } from "react"
import type { ProfileConfig } from "../../types"

interface Props {
  config: ProfileConfig
  onChange: (config: ProfileConfig) => void
  profileId?: string
}

export default function WeChatTab({ config, onChange, profileId }: Props) {
  const channel = config.channels.find((c) => c.type === "wechat")
  const getAuthHeaders = (): HeadersInit => {
    const headers: HeadersInit = { 'Content-Type': 'application/json' }
    const token = localStorage.getItem('octos_session_token')
      || localStorage.getItem('octos_auth_token')
    if (token) {
      headers['Authorization'] = `Bearer ${token}`
    }
    return headers
  }

  const [qrUrl, setQrUrl] = useState<string | null>(null)
  const [sessionKey, setSessionKey] = useState<string | null>(null)
  const [status, setStatus] = useState<string>("")
  const [error, setError] = useState<string>("")
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null)

  const token = config.env_vars?.WECHAT_BOT_TOKEN || ""
  const isConnected = Boolean(channel && token)

  useEffect(() => {
    return () => {
      if (pollRef.current) clearInterval(pollRef.current)
    }
  }, [])

  const startLogin = async () => {
    setError("")
    setStatus("loading")
    try {
      const id = profileId || "admin"
      const res = await fetch(`/api/my/profile/wechat/qr-start`, { headers: getAuthHeaders() })
      if (!res.ok) throw new Error(await res.text())
      const data = await res.json()
      setQrUrl(data.qrcode_url)
      setSessionKey(data.session_key)
      setStatus("scan")
      startPolling(data.session_key)
    } catch (e: unknown) {
      setError(String(e))
      setStatus("")
    }
  }

  const startPolling = (key: string) => {
    if (pollRef.current) clearInterval(pollRef.current)
    pollRef.current = setInterval(async () => {
      try {
        const id = profileId || "admin"
        const res = await fetch(`/api/my/profile/wechat/qr-poll`, {
          method: "POST",
          headers: getAuthHeaders(),
          body: JSON.stringify({ session_key: key }),
        })
        if (!res.ok) return
        const data = await res.json()
        if (data.status === "scaned") {
          setStatus("scanned")
        } else if (data.status === "confirmed") {
          setStatus("connected")
          setQrUrl(null)
          if (pollRef.current) clearInterval(pollRef.current)
        } else if (data.status === "expired") {
          setStatus("expired")
          setQrUrl(null)
          if (pollRef.current) clearInterval(pollRef.current)
        }
      } catch {
        // ignore poll errors
      }
    }, 2000)
  }

  const disconnect = () => {
    onChange({
      ...config,
      channels: config.channels.filter((c) => c.type !== "wechat"),
      env_vars: { ...config.env_vars, WECHAT_BOT_TOKEN: "" },
    })
    setStatus("")
    setQrUrl(null)
  }

  return (
    <div className="space-y-4">
      <div className="bg-gray-800/50 rounded-lg p-4 text-sm text-gray-300 space-y-2">
        <h3 className="text-white font-medium">WeChat Bot</h3>
        <p>
          Connect your WeChat account as a bot channel. Requires WeChat 8.0.70+ with OpenClaw support.
          Scan a QR code with WeChat to authorize.
        </p>
      </div>

      {isConnected && status !== "scan" && status !== "scanned" ? (
        <div className="space-y-3">
          <div className="flex items-center gap-2">
            <span className="w-2 h-2 bg-green-500 rounded-full" />
            <span className="text-green-400 text-sm">Connected</span>
          </div>
          <p className="text-xs text-gray-500">
            Token stored as WECHAT_BOT_TOKEN. If the session expires, click Reconnect below.
          </p>
          <div className="flex gap-2">
            <button
              onClick={startLogin}
              className="px-3 py-1.5 text-sm bg-gray-700 hover:bg-gray-600 rounded text-white"
            >
              Reconnect
            </button>
            <button
              onClick={disconnect}
              className="px-3 py-1.5 text-sm bg-red-900/50 hover:bg-red-800/50 rounded text-red-300"
            >
              Disconnect
            </button>
          </div>
        </div>
      ) : (
        <div className="space-y-3">
          {!qrUrl && status !== "scanned" && (
            <button
              onClick={startLogin}
              disabled={status === "loading"}
              className="px-4 py-2 bg-accent hover:bg-accent/80 rounded text-white text-sm disabled:opacity-50"
            >
              {status === "loading" ? "Loading..." : "Connect WeChat"}
            </button>
          )}

          {qrUrl && (
            <div className="space-y-2">
              <p className="text-sm text-gray-300">
                Scan this QR code with WeChat:
              </p>
              <div className="bg-white p-4 rounded-lg inline-block">
                <img
                  src={`https://api.qrserver.com/v1/create-qr-code/?size=200x200&data=${encodeURIComponent(qrUrl)}`}
                  alt="WeChat QR Code"
                  className="w-48 h-48"
                />
              </div>
              <p className="text-xs text-gray-500">
                Or open:{" "}
                <a href={qrUrl} target="_blank" rel="noopener" className="text-accent hover:underline">
                  {qrUrl.slice(0, 60)}...
                </a>
              </p>
            </div>
          )}

          {status === "scanned" && (
            <div className="flex items-center gap-2">
              <span className="w-2 h-2 bg-yellow-500 rounded-full animate-pulse" />
              <span className="text-yellow-400 text-sm">Scanned — confirm on your phone...</span>
            </div>
          )}

          {status === "expired" && (
            <div className="space-y-2">
              <p className="text-red-400 text-sm">QR code expired.</p>
              <button
                onClick={startLogin}
                className="px-3 py-1.5 text-sm bg-gray-700 hover:bg-gray-600 rounded text-white"
              >
                Get New QR Code
              </button>
            </div>
          )}

          {status === "connected" && (
            <div className="flex items-center gap-2">
              <span className="w-2 h-2 bg-green-500 rounded-full" />
              <span className="text-green-400 text-sm">Connected successfully!</span>
            </div>
          )}
        </div>
      )}

      {error && <p className="text-red-400 text-sm">{error}</p>}
    </div>
  )
}
