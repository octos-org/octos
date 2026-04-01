import { useAuth } from '../contexts/AuthContext'
import { useParams } from 'react-router-dom'

export default function AdminGuard({ children }: { children: React.ReactNode }) {
  const { isAdmin, user } = useAuth()
  const { id } = useParams<{ id: string }>()

  // Allow access if:
  // 1. User is admin, OR
  // 2. User is viewing their own profile, OR
  // 3. User is the parent of a sub-account (profile ID starts with their ID)
  const userId = user?.id || ''
  const isOwnProfile = id && (id === userId || id.startsWith(userId + '--'))

  if (!isAdmin && !isOwnProfile) {
    return (
      <div className="flex flex-col items-center justify-center h-64 text-center">
        <div className="text-4xl mb-4 text-gray-600">403</div>
        <h2 className="text-lg font-medium text-gray-300 mb-2">Access Denied</h2>
        <p className="text-sm text-gray-500">
          You need admin privileges to view this page.
        </p>
      </div>
    )
  }

  return <>{children}</>
}
