import { Routes, Route } from 'react-router-dom'
import { ToastProvider } from './components/Toast'
import { AuthProvider } from './contexts/AuthContext'
import AuthGuard from './components/AuthGuard'
import AdminGuard from './components/AdminGuard'
import Layout from './components/Layout'
import ProfileLayout from './layouts/ProfileLayout'
import Dashboard from './pages/Dashboard'
import NewProfile from './pages/NewProfile'
import LoginPage from './pages/LoginPage'
import UsersPage from './pages/UsersPage'
import AdminBotPage from './pages/AdminBotPage'
import ServerMetricsPage from './pages/ServerMetricsPage'
import HarnessPage from './pages/HarnessPage'
import { HomePage, LlmPage, MessagingPage, ToolsPage, SkillsPage, SystemPage } from './pages/profile'

export default function App() {
  return (
    <AuthProvider>
      <ToastProvider>
        <Routes>
          <Route path="login" element={<LoginPage />} />
          <Route element={<AuthGuard><Layout /></AuthGuard>}>
            {/* Global admin pages */}
            <Route index element={<Dashboard />} />
            <Route path="users" element={<AdminGuard><UsersPage /></AdminGuard>} />
            <Route path="admin-bot" element={<AdminGuard><AdminBotPage /></AdminGuard>} />
            <Route path="server" element={<AdminGuard><ServerMetricsPage /></AdminGuard>} />
            <Route path="harness" element={<AdminGuard><HarnessPage /></AdminGuard>} />
            <Route path="profiles/new" element={<AdminGuard><NewProfile /></AdminGuard>} />

            {/* Admin managing specific profile */}
            <Route path="profile/:id" element={<AdminGuard><ProfileLayout /></AdminGuard>}>
              <Route index element={<HomePage />} />
              <Route path="llm" element={<LlmPage />} />
              <Route path="messaging" element={<MessagingPage />} />
              <Route path="tools" element={<ToolsPage />} />
              <Route path="skills" element={<SkillsPage />} />
              <Route path="system" element={<SystemPage />} />
            </Route>

            {/* User's own profile */}
            <Route path="my" element={<ProfileLayout />}>
              <Route index element={<HomePage />} />
              <Route path="llm" element={<LlmPage />} />
              <Route path="messaging" element={<MessagingPage />} />
              <Route path="tools" element={<ToolsPage />} />
              <Route path="skills" element={<SkillsPage />} />
              <Route path="system" element={<SystemPage />} />
            </Route>
          </Route>
        </Routes>
      </ToastProvider>
    </AuthProvider>
  )
}
