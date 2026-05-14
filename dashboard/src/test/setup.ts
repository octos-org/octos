// Vitest setup — runs before each test file.
//
// Configures @testing-library/jest-dom matchers and provides minimal
// browser-shim defaults (localStorage, crypto.getRandomValues) so component
// tests can render without manual mocks.

import '@testing-library/jest-dom/vitest'
import { afterEach } from 'vitest'
import { cleanup } from '@testing-library/react'

afterEach(() => {
  cleanup()
  localStorage.clear()
})
