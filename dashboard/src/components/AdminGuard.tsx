import { useParams } from 'react-router-dom'
import { useAuth } from '../contexts/AuthContext'

export default function AdminGuard({ children }: { children: React.ReactNode }) {
  const { isAdmin, user, loading } = useAuth()
  const { id } = useParams<{ id: string }>()

  const userId = user?.id || ''
  const canAccessProfile = !!id && (id === userId || id.startsWith(`${userId}--`))

  if (loading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="animate-spin w-6 h-6 border-2 border-accent border-t-transparent rounded-full" />
      </div>
    )
  }

  if (!isAdmin && !canAccessProfile) {
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
