import { Navigate, Outlet } from 'react-router-dom'
import { useAuth } from '../contexts/AuthContext'

export default function AuthGuard({ children }: { children?: React.ReactNode }) {
  const { user, loading } = useAuth()

  if (loading) {
    return (
      <div className="flex items-center justify-center h-screen bg-background">
        <div className="animate-spin w-8 h-8 border-2 border-accent border-t-transparent rounded-full" />
      </div>
    )
  }

  if (!user) {
    // Redirect to the unified login page (octos-web SPA at /login).
    // Pass redirect param so the user returns to /admin/ after login.
    window.location.href = '/login?redirect=/admin/'
    return null
  }

  return children ? <>{children}</> : <Outlet />
}
