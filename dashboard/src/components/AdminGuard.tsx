import { useEffect, useState } from 'react'
import { useParams } from 'react-router-dom'

import { myApi } from '../api'
import { useAuth } from '../contexts/AuthContext'

export default function AdminGuard({ children }: { children: React.ReactNode }) {
  const { isAdmin, user } = useAuth()
  const { id } = useParams<{ id: string }>()
  const [loading, setLoading] = useState(false)
  const [allowed, setAllowed] = useState(false)

  useEffect(() => {
    let cancelled = false

    if (isAdmin) {
      setAllowed(true)
      setLoading(false)
      return () => {
        cancelled = true
      }
    }

    if (!id || !user) {
      setAllowed(false)
      setLoading(false)
      return () => {
        cancelled = true
      }
    }

    if (id === user.id) {
      setAllowed(true)
      setLoading(false)
      return () => {
        cancelled = true
      }
    }

    setLoading(true)
    setAllowed(false)

    myApi.listSubAccounts()
      .then((subs) => {
        if (!cancelled) {
          setAllowed(subs.some((sub) => sub.id === id))
        }
      })
      .catch(() => {
        if (!cancelled) {
          setAllowed(false)
        }
      })
      .finally(() => {
        if (!cancelled) {
          setLoading(false)
        }
      })

    return () => {
      cancelled = true
    }
  }, [id, isAdmin, user])

  if (loading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="animate-spin w-6 h-6 border-2 border-accent border-t-transparent rounded-full" />
      </div>
    )
  }

  if (!allowed) {
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
