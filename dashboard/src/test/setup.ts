// Vitest setup — runs before each test file.
//
// Configures @testing-library/jest-dom matchers and provides minimal
// browser-shim defaults (localStorage, crypto.getRandomValues) so component
// tests can render without manual mocks.

import '@testing-library/jest-dom/vitest'
import { afterEach } from 'vitest'
import { cleanup } from '@testing-library/react'

// Node >= 22 shadows jsdom's localStorage with an inert global that vitest's
// jsdom environment does not populate, so install a minimal in-memory Storage
// when none is reachable. A real jsdom/Node < 22 environment is left alone.
if (globalThis.localStorage == null) {
  const store = new Map<string, string>()
  const storage: Storage = {
    get length() {
      return store.size
    },
    key: (index: number) => Array.from(store.keys())[index] ?? null,
    getItem: (key: string) => (store.has(key) ? (store.get(key) as string) : null),
    setItem: (key: string, value: string) => {
      store.set(key, String(value))
    },
    removeItem: (key: string) => {
      store.delete(key)
    },
    clear: () => {
      store.clear()
    },
  }
  Object.defineProperty(globalThis, 'localStorage', { value: storage, configurable: true })
}

afterEach(() => {
  cleanup()
  localStorage.clear()
})
