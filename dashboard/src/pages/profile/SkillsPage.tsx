import { useState, useEffect, useCallback } from 'react'
import { useParams } from 'react-router-dom'
import { useAuth } from '../../contexts/AuthContext'
import { api, myApi } from '../../api'

interface SkillEntry {
  name: string
  version: string | null
  tool_count: number
  source_repo: string | null
}

export default function SkillsPage() {
  const { id } = useParams<{ id: string }>()
  const { isAdmin } = useAuth()
  const [skills, setSkills] = useState<SkillEntry[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  // Install form state
  const [repo, setRepo] = useState('')
  const [branch, setBranch] = useState('main')
  const [installing, setInstalling] = useState(false)
  const [installMsg, setInstallMsg] = useState<string | null>(null)

  // Removing state
  const [removing, setRemoving] = useState<string | null>(null)

  const ownProfile = !id

  const fetchSkills = useCallback(async () => {
    try {
      setError(null)
      const data = ownProfile
        ? await myApi.listProfileSkills()
        : await api.listProfileSkills(id)
      setSkills(data.skills || [])
    } catch (e: any) {
      setError(e.message)
    } finally {
      setLoading(false)
    }
  }, [id, ownProfile])

  useEffect(() => {
    fetchSkills()
  }, [fetchSkills])

  const handleInstall = async () => {
    if (!repo.trim()) return
    setInstalling(true)
    setInstallMsg(null)
    try {
      const result = ownProfile
        ? await myApi.installProfileSkill({
            repo: repo.trim(),
            force: false,
            branch,
          })
        : await api.installProfileSkill(id, {
            repo: repo.trim(),
            force: false,
            branch,
          })
      const installed = result.installed || []
      const skipped = result.skipped || []
      const msgs: string[] = []
      if (installed.length > 0) msgs.push(`Installed: ${installed.join(', ')}`)
      if (skipped.length > 0) msgs.push(`Skipped: ${skipped.join(', ')}`)
      setInstallMsg(msgs.join('. ') || 'Done')
      setRepo('')
      fetchSkills()
    } catch (e: any) {
      setInstallMsg(`Error: ${e.message}`)
    } finally {
      setInstalling(false)
    }
  }

  const handleRemove = async (name: string) => {
    if (!confirm(`Remove skill "${name}"?`)) return
    setRemoving(name)
    try {
      if (ownProfile) {
        await myApi.removeProfileSkill(name)
      } else {
        await api.removeProfileSkill(id, name)
      }
      fetchSkills()
    } catch (e: any) {
      setError(e.message)
    } finally {
      setRemoving(null)
    }
  }

  if (loading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="animate-spin w-6 h-6 border-2 border-accent border-t-transparent rounded-full" />
      </div>
    )
  }

  return (
    <div>
      <h1 className="text-2xl font-bold text-white mb-6">Skills</h1>

      {/* Installed skills */}
      <div className="bg-surface rounded-xl border border-gray-700/50 overflow-hidden mb-6">
        <div className="px-5 py-4 border-b border-gray-700/30">
          <h2 className="text-sm font-semibold text-gray-300">
            Installed Skills ({skills.length})
          </h2>
        </div>

        {error && (
          <div className="mx-5 mt-4 px-3 py-2 bg-red-500/10 border border-red-500/30 rounded text-sm text-red-400">
            {error}
          </div>
        )}

        {skills.length === 0 ? (
          <div className="px-5 py-8 text-center text-gray-500 text-sm">
            No skills installed. Install one from GitHub shorthand, a Git URL, or a local path below.
          </div>
        ) : (
          <div className="divide-y divide-gray-700/30">
            {skills.map((skill) => (
              <div
                key={skill.name}
                className="flex items-center justify-between px-5 py-3 hover:bg-white/[0.02] transition"
              >
                <div className="min-w-0">
                  <div className="flex items-center gap-2">
                    <span className="text-sm font-medium text-white">
                      {skill.name}
                    </span>
                    {skill.version && (
                      <span className="text-xs text-gray-500">
                        v{skill.version}
                      </span>
                    )}
                    {skill.tool_count > 0 && (
                      <span className="text-[10px] bg-accent/15 text-accent px-1.5 py-0.5 rounded-full">
                        {skill.tool_count} tool{skill.tool_count !== 1 ? 's' : ''}
                      </span>
                    )}
                  </div>
                  {skill.source_repo && (
                    <p className="text-xs text-gray-500 mt-0.5 truncate">
                      {skill.source_repo}
                    </p>
                  )}
                </div>

                {isAdmin && (
                  <button
                    onClick={() => handleRemove(skill.name)}
                    disabled={removing === skill.name}
                    className="ml-4 px-2.5 py-1 text-xs text-red-400 hover:text-red-300 hover:bg-red-500/10 rounded-lg transition disabled:opacity-50"
                  >
                    {removing === skill.name ? 'Removing...' : 'Remove'}
                  </button>
                )}
              </div>
            ))}
          </div>
        )}
      </div>

      {/* Install skill source */}
      {isAdmin && (
        <div className="bg-surface rounded-xl border border-gray-700/50 overflow-hidden">
          <div className="px-5 py-4 border-b border-gray-700/30">
            <h2 className="text-sm font-semibold text-gray-300">
              Install Skill
            </h2>
          </div>
          <div className="p-5 space-y-4">
            <div className="flex gap-3">
              <div className="flex-1">
                <label className="block text-xs text-gray-400 mb-1">
                  Source (GitHub shorthand, Git URL, or local path)
                </label>
                <input
                  type="text"
                  value={repo}
                  onChange={(e) => setRepo(e.target.value)}
                  placeholder="e.g. octos-org/system-skills, https://host/org/repo.git, or ./skills/my-skill"
                  className="w-full px-3 py-2 bg-black/30 border border-gray-700 rounded-lg text-sm text-white placeholder-gray-600 focus:outline-none focus:border-accent"
                  onKeyDown={(e) => e.key === 'Enter' && handleInstall()}
                />
              </div>
              <div className="w-28">
                <label className="block text-xs text-gray-400 mb-1">
                  Branch
                </label>
                <input
                  type="text"
                  value={branch}
                  onChange={(e) => setBranch(e.target.value)}
                  className="w-full px-3 py-2 bg-black/30 border border-gray-700 rounded-lg text-sm text-white focus:outline-none focus:border-accent"
                />
              </div>
            </div>
            <div className="flex items-center gap-3">
              <button
                onClick={handleInstall}
                disabled={installing || !repo.trim()}
                className="px-4 py-2 bg-accent hover:bg-accent/80 text-white text-sm font-medium rounded-lg transition disabled:opacity-50 disabled:cursor-not-allowed"
              >
                {installing ? 'Installing...' : 'Install'}
              </button>
              {installMsg && (
                <span
                  className={`text-xs ${
                    installMsg.startsWith('Error')
                      ? 'text-red-400'
                      : 'text-green-400'
                  }`}
                >
                  {installMsg}
                </span>
              )}
            </div>
            <p className="text-xs text-gray-600">
              Skills are installed to the profile&apos;s data directory. Accepted inputs: user/repo, user/repo/skill-name, full Git URL, or local path. Branch applies to Git installs. The gateway must be restarted to load new skills.
            </p>
          </div>
        </div>
      )}
    </div>
  )
}
