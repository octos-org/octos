/// <reference types="vitest" />
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

const base = process.env.VITE_BASE_PATH ?? '/swarm/'
const outDir = process.env.VITE_OUT_DIR ?? '../crates/octos-cli/static/swarm'

export default defineConfig({
  plugins: [react()],
  base,
  build: {
    outDir,
    emptyOutDir: true,
  },
  server: {
    port: 5174,
    proxy: {
      '/api': 'http://localhost:8080',
    },
  },
  test: {
    environment: 'jsdom',
    globals: true,
    setupFiles: ['./src/test/setup.ts'],
    // Pick up *.test.{ts,tsx} under src/ and tests/ so the main build
    // pipeline (`npm run build` + `typecheck`) never compiles tests.
    include: ['src/**/*.test.{ts,tsx}', 'tests/**/*.test.{ts,tsx}'],
  },
})
