import { useCallback, useEffect, useMemo, useState } from 'react'
import { api } from '../api'
import ConfirmDialog from '../components/ConfirmDialog'
import { useToast } from '../components/Toast'
import type { AllowlistEntry, User, UserRole } from '../types'

function formatDate(value: string | null | undefined): string {
  if (!value) return 'Never'
  const parsed = new Date(value)
  return Number.isNaN(parsed.getTime()) ? 'Unknown' : parsed.toLocaleDateString()
}

function registrationStatus(entry: AllowlistEntry): {
  label: string
  className: string
} {
  if (entry.registered) {
    return {
      label: 'Registered',
      className: 'bg-green-500/15 text-green-400',
    }
  }
  if (entry.claimed_user_id) {
    return {
      label: 'Claimed',
      className: 'bg-blue-500/15 text-blue-300',
    }
  }
  return {
    label: 'Pending',
    className: 'bg-amber-500/15 text-amber-300',
  }
}

type PendingConfirm =
  | { kind: 'allowlist'; entry: AllowlistEntry }
  | { kind: 'user'; user: User }

export default function UsersPage() {
  const { toast } = useToast()
  const [allowedEmails, setAllowedEmails] = useState<AllowlistEntry[]>([])
  const [users, setUsers] = useState<User[]>([])
  const [loading, setLoading] = useState(true)
  const [showCreate, setShowCreate] = useState(false)
  const [creating, setCreating] = useState(false)
  const [newEmail, setNewEmail] = useState('')
  const [newNote, setNewNote] = useState('')
  const [removingEmail, setRemovingEmail] = useState<string | null>(null)
  const [deletingId, setDeletingId] = useState<string | null>(null)
  const [pendingConfirm, setPendingConfirm] = useState<PendingConfirm | null>(null)
  const [selectedUserIds, setSelectedUserIds] = useState<Set<string>>(() => new Set())
  const [userSearch, setUserSearch] = useState('')
  const [roleFilter, setRoleFilter] = useState<UserRole | 'all'>('all')
  const [bulkDeleteOpen, setBulkDeleteOpen] = useState(false)
  const [bulkDeleting, setBulkDeleting] = useState(false)
  const [bulkErrors, setBulkErrors] = useState<string[]>([])
  const [updatingRoleId, setUpdatingRoleId] = useState<string | null>(null)

  const loadData = useCallback(async () => {
    try {
      const [allowedRes, usersRes] = await Promise.all([
        api.listAllowedEmails(),
        api.listUsers(),
      ])
      setAllowedEmails(
        [...allowedRes.entries].sort((a, b) => a.email.localeCompare(b.email)),
      )
      setUsers([...usersRes.users].sort((a, b) => a.email.localeCompare(b.email)))
    } catch (e: any) {
      toast(e.message, 'error')
    } finally {
      setLoading(false)
    }
  }, [toast])

  useEffect(() => {
    loadData()
  }, [loadData])

  useEffect(() => {
    const liveIds = new Set(users.map((user) => user.id))
    setSelectedUserIds((prev) => new Set([...prev].filter((id) => liveIds.has(id))))
  }, [users])

  const registeredIds = useMemo(
    () => new Set(allowedEmails.map((entry) => entry.registered_user_id).filter(Boolean)),
    [allowedEmails],
  )

  const registeredUsers = useMemo(
    () => users.filter((user) => registeredIds.has(user.id)),
    [registeredIds, users],
  )

  const otherUsers = useMemo(
    () => users.filter((user) => !registeredIds.has(user.id)),
    [registeredIds, users],
  )

  const accountRows = useMemo(() => [...registeredUsers, ...otherUsers], [otherUsers, registeredUsers])

  const filteredAccountRows = useMemo(() => {
    const query = userSearch.trim().toLowerCase()
    return accountRows.filter((user) => {
      const matchesRole = roleFilter === 'all' || user.role === roleFilter
      if (!matchesRole) return false
      if (!query) return true
      return (
        user.email.toLowerCase().includes(query)
        || user.name.toLowerCase().includes(query)
      )
    })
  }, [accountRows, roleFilter, userSearch])

  const selectedUsers = useMemo(
    () => accountRows.filter((user) => selectedUserIds.has(user.id)),
    [accountRows, selectedUserIds],
  )

  const visibleSelectedCount = filteredAccountRows.filter((user) => selectedUserIds.has(user.id)).length
  const allVisibleSelected = filteredAccountRows.length > 0 && visibleSelectedCount === filteredAccountRows.length

  const handleAllowEmail = async (e: React.FormEvent) => {
    e.preventDefault()
    try {
      setCreating(true)
      await api.addAllowedEmail({
        email: newEmail,
        note: newNote.trim() || undefined,
      })
      toast(`Allowed "${newEmail}" to sign in`)
      setShowCreate(false)
      setNewEmail('')
      setNewNote('')
      await loadData()
    } catch (e: any) {
      toast(e.message, 'error')
    } finally {
      setCreating(false)
    }
  }

  const removeAllowlistEntry = async (entry: AllowlistEntry) => {
    try {
      setRemovingEmail(entry.email)
      await api.deleteAllowedEmail(entry.email)
      toast(`Removed "${entry.email}" from the allowlist`)
      await loadData()
    } catch (e: any) {
      toast(e.message, 'error')
    } finally {
      setRemovingEmail(null)
    }
  }

  const deleteUser = async (user: User) => {
    try {
      setDeletingId(user.id)
      await api.deleteUser(user.id)
      toast(`Deleted account "${user.email}"`)
      await loadData()
    } catch (e: any) {
      toast(e.message, 'error')
    } finally {
      setDeletingId(null)
    }
  }

  const handleConfirm = async () => {
    const pending = pendingConfirm
    if (!pending) return
    setPendingConfirm(null)
    if (pending.kind === 'allowlist') {
      await removeAllowlistEntry(pending.entry)
    } else {
      await deleteUser(pending.user)
    }
  }

  const confirmTitle = pendingConfirm?.kind === 'user'
    ? 'Delete Account'
    : 'Remove Allowlisted Email'
  const confirmMessage = pendingConfirm?.kind === 'user'
    ? `Delete account "${pendingConfirm.user.email}"? This will also delete the profile and stop its gateway.`
    : pendingConfirm?.entry.registered
      ? `Remove "${pendingConfirm.entry.email}" from the allowlist? The already-registered account will remain, but future OTP signup will no longer be pre-authorized.`
      : pendingConfirm
        ? `Remove "${pendingConfirm.entry.email}" from the allowlist?`
        : ''
  const confirmLabel = pendingConfirm?.kind === 'user' ? 'Delete' : 'Remove'

  const toggleSelectedUser = (userId: string) => {
    setSelectedUserIds((prev) => {
      const next = new Set(prev)
      if (next.has(userId)) {
        next.delete(userId)
      } else {
        next.add(userId)
      }
      return next
    })
  }

  const toggleAllVisibleUsers = () => {
    setSelectedUserIds((prev) => {
      const next = new Set(prev)
      if (allVisibleSelected) {
        filteredAccountRows.forEach((user) => next.delete(user.id))
      } else {
        filteredAccountRows.forEach((user) => next.add(user.id))
      }
      return next
    })
  }

  const handleBulkDeleteUsers = async () => {
    const targets = selectedUsers
    if (targets.length === 0) return

    setBulkDeleting(true)
    setBulkErrors([])
    try {
      const results = await Promise.allSettled(
        targets.map(async (user) => {
          await api.deleteUser(user.id)
          return user
        }),
      )

      const failures: { user: User; message: string }[] = []
      results.forEach((result, index) => {
        if (result.status === 'rejected') {
          failures.push({
            user: targets[index],
            message: result.reason instanceof Error ? result.reason.message : String(result.reason),
          })
        }
      })

      const deletedCount = targets.length - failures.length
      if (failures.length > 0) {
        setBulkErrors(
          failures.map(({ user, message }) => `${user.email}: ${message}`),
        )
        setSelectedUserIds(new Set(failures.map(({ user }) => user.id)))
        toast(`${deletedCount} deleted, ${failures.length} failed`, 'error')
      } else {
        setSelectedUserIds(new Set())
        toast(`Deleted ${deletedCount} account${deletedCount === 1 ? '' : 's'}`)
      }

      await loadData()
    } catch (e: any) {
      setBulkErrors([e.message])
      toast(e.message, 'error')
    } finally {
      setBulkDeleting(false)
      setBulkDeleteOpen(false)
    }
  }

  const handleChangeRole = async (user: User) => {
    const nextRole: UserRole = user.role === 'admin' ? 'user' : 'admin'
    const action = nextRole === 'admin' ? 'Promote' : 'Demote'
    const target = nextRole === 'admin' ? 'admin' : 'regular user'
    if (!confirm(`${action} account "${user.email}" to ${target}?`)) {
      return
    }
    try {
      setUpdatingRoleId(user.id)
      await api.updateUserRole(user.id, nextRole)
      toast(`${action}d account "${user.email}"`)
      await loadData()
    } catch (e: any) {
      toast(e.message, 'error')
    } finally {
      setUpdatingRoleId(null)
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
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl font-bold text-white">Access</h1>
          <p className="text-sm text-gray-500 mt-1">
            {allowedEmails.length} allowlisted email{allowedEmails.length === 1 ? '' : 's'}
            <span className="text-gray-600 ml-2">
              {registeredUsers.length} claimed
            </span>
          </p>
        </div>
        <button
          onClick={() => setShowCreate(true)}
          className="px-4 py-2 text-sm font-medium rounded-lg bg-accent text-white hover:bg-accent-light transition"
        >
          + Allow Email
        </button>
      </div>

      <div className="bg-surface rounded-xl border border-gray-700/50 p-5 mb-6">
        <h2 className="text-sm font-medium text-white mb-1">How access works now</h2>
        <p className="text-sm text-gray-400 leading-6">
          Allowlisting an email pre-authorizes OTP signup without pre-creating the full account.
          The actual user/profile record is created or claimed when that email completes login.
        </p>
      </div>

      {showCreate && (
        <div className="bg-surface rounded-xl border border-gray-700/50 p-5 mb-6">
          <h3 className="text-sm font-medium text-white mb-4">Allow email</h3>
          <form onSubmit={handleAllowEmail} className="space-y-3">
            <div>
              <label className="block text-xs text-gray-500 mb-1">Email</label>
              <input
                type="email"
                value={newEmail}
                onChange={(e) => setNewEmail(e.target.value)}
                placeholder="user@example.com"
                className="input text-sm"
                required
                autoFocus
              />
            </div>
            <div>
              <label className="block text-xs text-gray-500 mb-1">Note (optional)</label>
              <input
                value={newNote}
                onChange={(e) => setNewNote(e.target.value)}
                placeholder="Sales, pilot cohort, contractor, etc."
                className="input text-sm"
              />
            </div>
            <div className="flex gap-2 pt-2">
              <button
                type="submit"
                disabled={creating}
                className="px-4 py-2 text-sm font-medium rounded-lg bg-accent text-white hover:bg-accent-light transition disabled:opacity-50"
              >
                {creating ? 'Saving...' : 'Allow Email'}
              </button>
              <button
                type="button"
                onClick={() => setShowCreate(false)}
                className="px-4 py-2 text-sm font-medium text-gray-400 hover:text-white rounded-lg hover:bg-white/5 transition"
              >
                Cancel
              </button>
            </div>
          </form>
        </div>
      )}

      <div className="bg-surface rounded-xl border border-gray-700/50 overflow-hidden mb-6">
        <div className="px-4 py-3 border-b border-gray-700/50">
          <h2 className="text-sm font-medium text-white">Allowlisted emails</h2>
          <p className="text-xs text-gray-500 mt-1">
            These addresses can complete OTP signup later. Registration happens on first successful login.
          </p>
        </div>
        {allowedEmails.length > 0 ? (
          <table className="w-full">
            <thead>
              <tr className="border-b border-gray-700/50">
                <th className="text-left px-4 py-3 text-xs font-medium text-gray-500 uppercase">Email</th>
                <th className="text-left px-4 py-3 text-xs font-medium text-gray-500 uppercase">Note</th>
                <th className="text-left px-4 py-3 text-xs font-medium text-gray-500 uppercase">Status</th>
                <th className="text-left px-4 py-3 text-xs font-medium text-gray-500 uppercase">Registered Account</th>
                <th className="text-left px-4 py-3 text-xs font-medium text-gray-500 uppercase">Last Login</th>
                <th className="text-right px-4 py-3 text-xs font-medium text-gray-500 uppercase">Actions</th>
              </tr>
            </thead>
            <tbody>
              {allowedEmails.map((entry) => {
                const status = registrationStatus(entry)
                return (
                  <tr key={entry.email} className="border-b border-gray-700/30 last:border-0">
                    <td className="px-4 py-3 text-sm text-white font-mono">{entry.email}</td>
                    <td className="px-4 py-3 text-sm text-gray-300">{entry.note || '—'}</td>
                    <td className="px-4 py-3">
                      <span className={`inline-flex px-2 py-0.5 text-[10px] font-medium rounded-full ${status.className}`}>
                        {status.label}
                      </span>
                    </td>
                    <td className="px-4 py-3 text-sm text-gray-300">
                      {entry.registered_name || entry.registered_user_id || '—'}
                    </td>
                    <td className="px-4 py-3 text-xs text-gray-500">
                      {formatDate(entry.last_login_at)}
                    </td>
                    <td className="px-4 py-3 text-right">
                      <button
                        onClick={() => setPendingConfirm({ kind: 'allowlist', entry })}
                        disabled={removingEmail === entry.email}
                        className="text-xs text-red-400 hover:text-red-300 disabled:opacity-50"
                      >
                        {removingEmail === entry.email ? 'Removing...' : 'Remove'}
                      </button>
                    </td>
                  </tr>
                )
              })}
            </tbody>
          </table>
        ) : (
          <div className="px-4 py-10 text-center">
            <h3 className="text-lg font-medium text-gray-400 mb-2">No allowlisted emails yet</h3>
            <p className="text-sm text-gray-500">
              Add an email here to pre-authorize OTP signup without creating the account up front.
            </p>
          </div>
        )}
      </div>

      <div className="bg-surface rounded-xl border border-gray-700/50 overflow-hidden">
        <div className="px-4 py-3 border-b border-gray-700/50 space-y-3">
          <div>
            <h2 className="text-sm font-medium text-white">Registered accounts</h2>
            <p className="text-xs text-gray-500 mt-1">
              Real accounts that already exist. These are shown for visibility; allowlist management above is the primary signup workflow.
            </p>
          </div>
          <div className="flex flex-col gap-3 lg:flex-row lg:items-end lg:justify-between">
            <div className="grid gap-3 sm:grid-cols-[minmax(0,1fr)_160px] lg:w-[520px]">
              <label className="block">
                <span className="block text-xs text-gray-500 mb-1">Search users</span>
                <input
                  value={userSearch}
                  onChange={(e) => setUserSearch(e.target.value)}
                  placeholder="Email or name"
                  className="input text-sm"
                />
              </label>
              <label className="block">
                <span className="block text-xs text-gray-500 mb-1">Role</span>
                <select
                  value={roleFilter}
                  onChange={(e) => setRoleFilter(e.target.value as UserRole | 'all')}
                  className="input text-sm"
                >
                  <option value="all">All roles</option>
                  <option value="admin">Admins</option>
                  <option value="user">Users</option>
                </select>
              </label>
            </div>
            <div className="flex flex-wrap items-center gap-3">
              <span className="text-xs text-gray-500">
                {selectedUsers.length} selected
              </span>
              <button
                type="button"
                onClick={() => setBulkDeleteOpen(true)}
                disabled={selectedUsers.length === 0 || bulkDeleting}
                className="px-3 py-2 text-xs font-medium rounded-lg border border-red-500/30 text-red-300 bg-red-500/10 hover:bg-red-500/20 transition disabled:opacity-50 disabled:cursor-not-allowed"
              >
                {bulkDeleting ? 'Deleting...' : 'Delete Selected'}
              </button>
            </div>
          </div>
          {bulkErrors.length > 0 && (
            <div role="alert" className="rounded-lg border border-red-500/30 bg-red-500/10 p-3">
              <h3 className="text-xs font-medium text-red-300 mb-2">Bulk delete errors</h3>
              <ul className="space-y-1">
                {bulkErrors.map((error) => (
                  <li key={error} className="text-xs text-red-200">{error}</li>
                ))}
              </ul>
            </div>
          )}
        </div>
        {filteredAccountRows.length > 0 ? (
          <table className="w-full">
            <thead>
              <tr className="border-b border-gray-700/50">
                <th className="text-left px-4 py-3 text-xs font-medium text-gray-500 uppercase w-10">
                  <input
                    type="checkbox"
                    aria-label="Select all visible users"
                    checked={allVisibleSelected}
                    onChange={toggleAllVisibleUsers}
                    disabled={filteredAccountRows.length === 0}
                    className="h-4 w-4 rounded border-gray-600 bg-gray-900 text-accent focus:ring-accent"
                  />
                </th>
                <th className="text-left px-4 py-3 text-xs font-medium text-gray-500 uppercase">Email</th>
                <th className="text-left px-4 py-3 text-xs font-medium text-gray-500 uppercase">Name</th>
                <th className="text-left px-4 py-3 text-xs font-medium text-gray-500 uppercase">Role</th>
                <th className="text-left px-4 py-3 text-xs font-medium text-gray-500 uppercase">Created</th>
                <th className="text-left px-4 py-3 text-xs font-medium text-gray-500 uppercase">Last Login</th>
                <th className="text-right px-4 py-3 text-xs font-medium text-gray-500 uppercase">Actions</th>
              </tr>
            </thead>
            <tbody>
              {filteredAccountRows.map((user) => (
                <tr key={user.id} className="border-b border-gray-700/30 last:border-0">
                  <td className="px-4 py-3">
                    <input
                      type="checkbox"
                      aria-label={`Select user ${user.email}`}
                      checked={selectedUserIds.has(user.id)}
                      onChange={() => toggleSelectedUser(user.id)}
                      className="h-4 w-4 rounded border-gray-600 bg-gray-900 text-accent focus:ring-accent"
                    />
                  </td>
                  <td className="px-4 py-3 text-sm text-white font-mono">{user.email}</td>
                  <td className="px-4 py-3 text-sm text-gray-300">{user.name}</td>
                  <td className="px-4 py-3">
                    <span
                      className={`inline-flex px-2 py-0.5 text-[10px] font-medium rounded-full ${
                        user.role === 'admin'
                          ? 'bg-amber-500/15 text-amber-400'
                          : 'bg-gray-500/15 text-gray-400'
                      }`}
                    >
                      {user.role}
                    </span>
                  </td>
                  <td className="px-4 py-3 text-xs text-gray-500">
                    {formatDate(user.created_at)}
                  </td>
                  <td className="px-4 py-3 text-xs text-gray-500">
                    {formatDate(user.last_login_at)}
                  </td>
                  <td className="px-4 py-3 text-right">
                    <div className="flex justify-end gap-3">
                      <button
                        onClick={() => handleChangeRole(user)}
                        disabled={updatingRoleId === user.id}
                        className="text-xs text-amber-300 hover:text-amber-200 disabled:opacity-50"
                      >
                        {updatingRoleId === user.id
                          ? 'Updating...'
                          : user.role === 'admin'
                            ? 'Demote'
                            : 'Promote'}
                      </button>
                      <button
                        onClick={() => setPendingConfirm({ kind: 'user', user })}
                        disabled={deletingId === user.id}
                        className="text-xs text-red-400 hover:text-red-300 disabled:opacity-50"
                      >
                        {deletingId === user.id ? 'Deleting...' : 'Delete'}
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        ) : users.length > 0 ? (
          <div className="px-4 py-10 text-center">
            <h3 className="text-lg font-medium text-gray-400 mb-2">No accounts match the filters</h3>
            <p className="text-sm text-gray-500">
              Adjust the search text or role filter to show more registered accounts.
            </p>
          </div>
        ) : (
          <div className="px-4 py-10 text-center">
            <h3 className="text-lg font-medium text-gray-400 mb-2">No registered accounts yet</h3>
            <p className="text-sm text-gray-500">
              Accounts will appear here after an allowlisted or already-registered email completes OTP login.
            </p>
          </div>
        )}
      </div>
      <ConfirmDialog
        open={pendingConfirm !== null}
        title={confirmTitle}
        message={confirmMessage}
        confirmLabel={confirmLabel}
        danger
        onConfirm={handleConfirm}
        onCancel={() => setPendingConfirm(null)}
      />
      <ConfirmDialog
        open={bulkDeleteOpen}
        title="Delete selected accounts?"
        message={`Delete ${selectedUsers.length} selected account${selectedUsers.length === 1 ? '' : 's'}? This will also delete their profiles and stop their gateways.`}
        confirmLabel="Delete"
        danger
        onCancel={() => setBulkDeleteOpen(false)}
        onConfirm={handleBulkDeleteUsers}
      />
    </div>
  )
}
