/// <reference types="vitest" />
import { defineConfig } from 'vitest/config';

// Layer 1 fixture replay tests. Pure logic, no DOM — node environment.
// Target: <1s total runtime, deterministic, no external services.
export default defineConfig({
  test: {
    environment: 'node',
    globals: false,
    include: ['src/state/__tests__/**/*.test.ts'],
    // The fixture-replay engine is allocation-light and entirely
    // synchronous; a single thread is faster than the pool overhead.
    pool: 'threads',
    poolOptions: { threads: { singleThread: true } },
    // Fail fast on accidental CI hangs.
    testTimeout: 5_000,
    // Reporters: keep CI logs minimal; use verbose locally via `npx vitest`.
    reporters: process.env.CI ? ['default'] : ['verbose'],
  },
});
