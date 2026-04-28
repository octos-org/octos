import SwarmPage from './pages/SwarmPage'

/**
 * Standalone swarm-app shell. Hosts the 4 M7.6 panels directly — no
 * routing, no profile selector, no admin chrome. This app is a siblings
 * to the admin dashboard and is intended for PMs + supervisors whose
 * whole job is the swarm dispatch surface.
 */
export default function App() {
  return (
    <div className="min-h-screen bg-bg text-gray-100">
      <header className="border-b border-gray-700/50 bg-surface px-6 py-4">
        <div className="mx-auto flex max-w-6xl items-center justify-between gap-4">
          <div>
            <h1 className="text-lg font-bold tracking-tight">
              <span className="text-accent">octos</span> swarm
            </h1>
            <p className="mt-0.5 text-xs text-gray-500">
              PM + supervisor orchestrator — author contracts, dispatch
              swarms, watch live progress, gate decisions.
            </p>
          </div>
        </div>
      </header>
      <main className="mx-auto max-w-6xl px-6 py-6">
        <SwarmPage />
      </main>
    </div>
  )
}
