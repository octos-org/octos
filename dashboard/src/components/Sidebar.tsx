import { NavLink, useMatch, Link } from 'react-router-dom'
import { useAuth } from '../contexts/AuthContext'

export default function Sidebar() {
  const { user, isAdmin, logout } = useAuth()

  // Detect if we're on a profile route
  const adminProfileMatch = useMatch('/profile/:id/*')
  const myProfileMatch = useMatch('/my/*')
  const isProfileMode = !!(adminProfileMatch || myProfileMatch)

  // Build the base path for profile navigation links
  const profileBase = adminProfileMatch
    ? `/profile/${adminProfileMatch.params.id}`
    : '/my'

  return (
    <aside className="fixed top-0 left-0 w-56 h-screen bg-surface border-r border-gray-700/50 flex flex-col z-10">
      {/* Brand */}
      <div className="px-4 py-5 border-b border-gray-700/50">
        <h1 className="text-lg font-bold text-white tracking-tight">
          <span className="text-accent">crew-rs</span>
          {isAdmin ? ' Admin' : ''}
        </h1>
      </div>

      <nav className="flex-1 px-3 py-4 space-y-1 overflow-y-auto">
        {/* Admin global links — always visible for admins */}
        {isAdmin && (
          <>
            <SidebarLink to="/" end icon={<GridIcon />} label="Dashboard" />
            <SidebarLink to="/my" icon={<UserIcon />} label="My Profile" />
            {/* My Profile sub-nav — nested tree view */}
            {myProfileMatch && (
              <div className="ml-4 pl-3 border-l border-gray-700/40 space-y-0.5">
                <SidebarLink to="/my" end icon={<HomeIcon />} label="Home" />
                <SidebarLink to="/my/llm" icon={<CpuIcon />} label="LLM Providers" />
                <SidebarLink to="/my/messaging" icon={<MessageIcon />} label="Messaging" />
                <SidebarLink to="/my/tools" icon={<WrenchIcon />} label="Tools" />
                <SidebarLink to="/my/system" icon={<CogIcon />} label="System" />
              </div>
            )}
            <SidebarLink to="/users" icon={<UsersIcon />} label="Users" />
            <SidebarLink to="/admin-bot" icon={<BotIcon />} label="Admin Bot" />
            <SidebarLink to="/server" icon={<PulseIcon />} label="Server" />
          </>
        )}

        {/* Non-admin: flat profile nav */}
        {!isAdmin && (
          <>
            <SidebarLink to="/my" end icon={<HomeIcon />} label="Home" />
            <SidebarLink to="/my/llm" icon={<CpuIcon />} label="LLM Providers" />
            <SidebarLink to="/my/messaging" icon={<MessageIcon />} label="Messaging" />
            <SidebarLink to="/my/tools" icon={<WrenchIcon />} label="Tools" />
            <SidebarLink to="/my/system" icon={<CogIcon />} label="System" />
          </>
        )}

        {/* Admin viewing another profile — tree nested under separator */}
        {isAdmin && adminProfileMatch && (
          <>
            <div className="border-t border-gray-700/30 my-3" />
            <p className="px-3 py-1 text-[10px] font-semibold text-gray-500 uppercase tracking-wider">
              {adminProfileMatch.params.id}
            </p>
            <div className="ml-1 space-y-0.5">
              <SidebarLink to={profileBase} end icon={<HomeIcon />} label="Home" />
              <SidebarLink to={`${profileBase}/llm`} icon={<CpuIcon />} label="LLM Providers" />
              <SidebarLink to={`${profileBase}/messaging`} icon={<MessageIcon />} label="Messaging" />
              <SidebarLink to={`${profileBase}/tools`} icon={<WrenchIcon />} label="Tools" />
              <SidebarLink to={`${profileBase}/system`} icon={<CogIcon />} label="System" />
            </div>
          </>
        )}
      </nav>

      {/* User info + logout */}
      <div className="px-4 py-3 border-t border-gray-700/50">
        {user && (
          <div className="flex items-center justify-between">
            <div className="min-w-0">
              <p className="text-xs text-gray-300 truncate">{user.name || user.email}</p>
              <p className="text-[10px] text-gray-600 truncate">{user.email}</p>
            </div>
            <button
              onClick={logout}
              className="ml-2 p-1.5 text-gray-500 hover:text-gray-300 rounded-lg hover:bg-white/5 transition"
              title="Logout"
            >
              <LogoutIcon />
            </button>
          </div>
        )}
        {!user && (
          <p className="text-xs text-gray-500">crew-rs v0.1.0</p>
        )}
      </div>
    </aside>
  )
}

function SidebarLink({ to, end, icon, label }: { to: string; end?: boolean; icon: React.ReactNode; label: string }) {
  return (
    <NavLink
      to={to}
      end={end}
      className={({ isActive }) =>
        `flex items-center gap-3 px-3 py-2 rounded-lg text-sm font-medium transition-colors ${
          isActive
            ? 'bg-accent/15 text-accent'
            : 'text-gray-400 hover:text-gray-200 hover:bg-white/5'
        }`
      }
    >
      {icon}
      {label}
    </NavLink>
  )
}

// ── Icons ──

function ChevronLeftIcon() {
  return (
    <svg className="w-3.5 h-3.5" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
      <path strokeLinecap="round" strokeLinejoin="round" d="M15 19l-7-7 7-7" />
    </svg>
  )
}

function HomeIcon() {
  return (
    <svg className="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
      <path strokeLinecap="round" strokeLinejoin="round" d="M3 12l2-2m0 0l7-7 7 7M5 10v10a1 1 0 001 1h3m10-11l2 2m-2-2v10a1 1 0 01-1 1h-3m-6 0a1 1 0 001-1v-4a1 1 0 011-1h2a1 1 0 011 1v4a1 1 0 001 1m-6 0h6" />
    </svg>
  )
}

function CpuIcon() {
  return (
    <svg className="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
      <path strokeLinecap="round" strokeLinejoin="round" d="M9 3v2m6-2v2M9 19v2m6-2v2M5 9H3m2 6H3m18-6h-2m2 6h-2M7 19h10a2 2 0 002-2V7a2 2 0 00-2-2H7a2 2 0 00-2 2v10a2 2 0 002 2zM9 9h6v6H9V9z" />
    </svg>
  )
}

function MessageIcon() {
  return (
    <svg className="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
      <path strokeLinecap="round" strokeLinejoin="round" d="M8 12h.01M12 12h.01M16 12h.01M21 12c0 4.418-4.03 8-9 8a9.863 9.863 0 01-4.255-.949L3 20l1.395-3.72C3.512 15.042 3 13.574 3 12c0-4.418 4.03-8 9-8s9 3.582 9 8z" />
    </svg>
  )
}

function WrenchIcon() {
  return (
    <svg className="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
      <path strokeLinecap="round" strokeLinejoin="round" d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.066 2.573c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.573 1.066c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.066-2.573c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z" />
      <path strokeLinecap="round" strokeLinejoin="round" d="M15 12a3 3 0 11-6 0 3 3 0 016 0z" />
    </svg>
  )
}

function CogIcon() {
  return (
    <svg className="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
      <path strokeLinecap="round" strokeLinejoin="round" d="M5 12h14M5 12a2 2 0 01-2-2V6a2 2 0 012-2h14a2 2 0 012 2v4a2 2 0 01-2 2M5 12a2 2 0 00-2 2v4a2 2 0 002 2h14a2 2 0 002-2v-4a2 2 0 00-2-2m-2-4h.01M17 16h.01" />
    </svg>
  )
}

function GridIcon() {
  return (
    <svg className="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
      <path strokeLinecap="round" strokeLinejoin="round" d="M4 6a2 2 0 012-2h2a2 2 0 012 2v2a2 2 0 01-2 2H6a2 2 0 01-2-2V6zM14 6a2 2 0 012-2h2a2 2 0 012 2v2a2 2 0 01-2 2h-2a2 2 0 01-2-2V6zM4 16a2 2 0 012-2h2a2 2 0 012 2v2a2 2 0 01-2 2H6a2 2 0 01-2-2v-2zM14 16a2 2 0 012-2h2a2 2 0 012 2v2a2 2 0 01-2 2h-2a2 2 0 01-2-2v-2z" />
    </svg>
  )
}

function UserIcon() {
  return (
    <svg className="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
      <path strokeLinecap="round" strokeLinejoin="round" d="M16 7a4 4 0 11-8 0 4 4 0 018 0zM12 14a7 7 0 00-7 7h14a7 7 0 00-7-7z" />
    </svg>
  )
}

function UsersIcon() {
  return (
    <svg className="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
      <path strokeLinecap="round" strokeLinejoin="round" d="M12 4.354a4 4 0 110 5.292M15 21H3v-1a6 6 0 0112 0v1zm0 0h6v-1a6 6 0 00-9-5.197M13 7a4 4 0 11-8 0 4 4 0 018 0z" />
    </svg>
  )
}

function BotIcon() {
  return (
    <svg className="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
      <path strokeLinecap="round" strokeLinejoin="round" d="M9.75 3.104v5.714a2.25 2.25 0 01-.659 1.591L5 14.5M9.75 3.104c-.251.023-.501.05-.75.082m.75-.082a24.301 24.301 0 014.5 0m0 0v5.714a2.25 2.25 0 00.659 1.591L19 14.5M14.25 3.104c.251.023.501.05.75.082M19 14.5l-1.5 4.5H6.5L5 14.5m14 0H5" />
    </svg>
  )
}

function PulseIcon() {
  return (
    <svg className="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
      <path strokeLinecap="round" strokeLinejoin="round" d="M3 12h4l3-9 4 18 3-9h4" />
    </svg>
  )
}

function LogoutIcon() {
  return (
    <svg className="w-3.5 h-3.5" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
      <path strokeLinecap="round" strokeLinejoin="round" d="M17 16l4-4m0 0l-4-4m4 4H7m6 4v1a3 3 0 01-3 3H6a3 3 0 01-3-3V7a3 3 0 013-3h4a3 3 0 013 3v1" />
    </svg>
  )
}
