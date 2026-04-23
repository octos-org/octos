/// <reference types="vitest" />
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

const base = process.env.VITE_BASE_PATH ?? '/admin/'
const outDir = process.env.VITE_OUT_DIR ?? '../crates/octos-cli/static/admin'

export default defineConfig({
  plugins: [react()],
  base,
  build: {
    outDir,
    emptyOutDir: true,
  },
  server: {
    port: 5173,
    proxy: {
      '/api': 'http://localhost:8080',
    },
  },
  test: {
    environment: 'jsdom',
    globals: true,
    setupFiles: ['./src/test/setup.ts'],
    // Only pick up .test.tsx files so the main build pipeline
    // (`npm run build` + `typecheck`) never tries to compile tests.
    include: ['src/**/*.test.{ts,tsx}'],
  },
})
