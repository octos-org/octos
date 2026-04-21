import { useState, useEffect, useCallback } from 'react'
import { useParams } from 'react-router-dom'
import { useAuth } from '../../contexts/AuthContext'
import { api, myApi, type SkillRegistryPackage } from '../../api'

interface SkillEntry {
  name: string
  version: string | null
  tool_count: number
  source_repo: string | null
}

type InstallSourceMode = 'github' | 'octos-hub'

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
  const [sourceMode, setSourceMode] = useState<InstallSourceMode>('github')
  const [registryPackages, setRegistryPackages] = useState<SkillRegistryPackage[]>([])
  const [loadingRegistry, setLoadingRegistry] = useState(false)
  const [registryError, setRegistryError] = useState<string | null>(null)
  const [hubQuery, setHubQuery] = useState('')

  // Removing state
  const [removing, setRemoving] = useState<string | null>(null)

  const ownProfile = !id
  const canManageSkills = isAdmin || ownProfile
  const showOctosHubInstall = ownProfile

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

  const fetchRegistry = useCallback(async () => {
    if (!canManageSkills || !showOctosHubInstall) return
    setLoadingRegistry(true)
    setRegistryError(null)
    try {
      const data = await myApi.listProfileSkillRegistry()
      setRegistryPackages(data.packages || [])
    } catch (e: any) {
      setRegistryError(e.message || 'Failed to load Octos Hub registry')
    } finally {
      setLoadingRegistry(false)
    }
  }, [canManageSkills, showOctosHubInstall])

  useEffect(() => {
    fetchRegistry()
  }, [fetchRegistry])

  useEffect(() => {
    if (!showOctosHubInstall && sourceMode === 'octos-hub') {
      setSourceMode('github')
    }
  }, [showOctosHubInstall, sourceMode])

  const handleInstallFromSource = async (source: string) => {
    if (!source.trim()) return
    setInstalling(true)
    setInstallMsg(null)
    try {
      const result = ownProfile
        ? await myApi.installProfileSkill({
            repo: source.trim(),
            force: false,
            branch,
          })
        : await api.installProfileSkill(id, {
            repo: source.trim(),
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

  const handleInstall = async () => {
    await handleInstallFromSource(repo)
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

  const filteredRegistryPackages = registryPackages.filter((pkg) => {
    const q = hubQuery.trim().toLowerCase()
    if (!q) return true
    return (
      pkg.name.toLowerCase().includes(q)
      || pkg.description.toLowerCase().includes(q)
      || pkg.repo.toLowerCase().includes(q)
      || pkg.skills.some((skill) => skill.toLowerCase().includes(q))
      || pkg.tags.some((tag) => tag.toLowerCase().includes(q))
    )
  })

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

                {canManageSkills && (
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
      {canManageSkills && (
        <div className="bg-surface rounded-xl border border-gray-700/50 overflow-hidden">
          <div className="px-5 py-4 border-b border-gray-700/30">
            <h2 className="text-sm font-semibold text-gray-300">
              Install Skill
            </h2>
          </div>
          <div className="p-5 space-y-4">
            <div className="inline-flex rounded-lg border border-gray-700/70 bg-black/20 p-1">
              <button
                type="button"
                onClick={() => setSourceMode('github')}
                className={`px-3 py-1.5 text-xs rounded-md transition ${
                  sourceMode === 'github'
                    ? 'bg-accent text-white'
                    : 'text-gray-300 hover:bg-white/[0.06]'
                }`}
              >
                GitHub / Git URL / Local Path
              </button>
              {showOctosHubInstall && (
                <button
                  type="button"
                  onClick={() => setSourceMode('octos-hub')}
                  className={`px-3 py-1.5 text-xs rounded-md transition ${
                    sourceMode === 'octos-hub'
                      ? 'bg-accent text-white'
                      : 'text-gray-300 hover:bg-white/[0.06]'
                  }`}
                >
                  Octos Hub
                </button>
              )}
            </div>

            {sourceMode === 'github' || !showOctosHubInstall ? (
              <>
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
                </div>
              </>
            ) : (
              <div className="space-y-3">
                <div>
                  <label className="block text-xs text-gray-400 mb-1">
                    Search Octos Hub packages
                  </label>
                  <input
                    type="text"
                    value={hubQuery}
                    onChange={(e) => setHubQuery(e.target.value)}
                    placeholder="e.g. mofa, slides, podcast"
                    className="w-full px-3 py-2 bg-black/30 border border-gray-700 rounded-lg text-sm text-white placeholder-gray-600 focus:outline-none focus:border-accent"
                  />
                </div>
                {loadingRegistry ? (
                  <div className="text-xs text-gray-400">Loading Octos Hub packages...</div>
                ) : registryError ? (
                  <div className="text-xs text-red-400">{registryError}</div>
                ) : filteredRegistryPackages.length === 0 ? (
                  <div className="text-xs text-gray-500">
                    No matching packages in Octos Hub.
                  </div>
                ) : (
                  <div className="max-h-64 overflow-auto rounded-lg border border-gray-700/40 divide-y divide-gray-700/30">
                    {filteredRegistryPackages.map((pkg) => (
                      <div key={pkg.repo} className="px-3 py-2.5 flex items-start justify-between gap-3">
                        <div className="min-w-0">
                          <div className="text-sm text-white font-medium">
                            {pkg.name}
                            {pkg.version ? (
                              <span className="ml-2 text-[10px] text-gray-400">v{pkg.version}</span>
                            ) : null}
                          </div>
                          <p className="text-xs text-gray-400 truncate">{pkg.description}</p>
                          <p className="text-[11px] text-gray-500 truncate">{pkg.repo}</p>
                        </div>
                        <button
                          type="button"
                          onClick={() => handleInstallFromSource(pkg.repo)}
                          disabled={installing}
                          className="shrink-0 px-3 py-1.5 bg-accent hover:bg-accent/80 text-white text-xs font-medium rounded-lg transition disabled:opacity-50"
                        >
                          {installing ? 'Installing...' : 'Install'}
                        </button>
                      </div>
                    ))}
                  </div>
                )}
              </div>
            )}
            <div className="flex items-center gap-3">
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
              Skills are installed to the profile&apos;s data directory. Use GitHub/Git URL/local path directly or pick a package from Octos Hub. The gateway must be restarted to load new skills.
            </p>
          </div>
        </div>
      )}
    </div>
  )
}
