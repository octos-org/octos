import { defineConfig } from '@playwright/test';

const INCLUDE_LIVE_E2E = process.env.OCTOS_E2E_LIVE === '1';

const LIVE_E2E_TESTS = [
  '**/live-*.spec.ts',
  '**/*-live.spec.ts',
  '**/fleet-*.spec.ts',
  '**/mini5-*.spec.ts',
  '**/round*-fleet-*.spec.ts',
  '**/background-task-header-switching.spec.ts',
  '**/coding-hardcases.spec.ts',
  '**/refactor-capabilities.spec.ts',
  '**/runtime-regression.spec.ts',
  '**/session-recovery.spec.ts',
  '**/skill-compat-gate.spec.ts',
];

/**
 * E2E tests for the octos web client + API.
 *
 * Prerequisites:
 *   cargo build --release -p octos-cli --features "octos-cli/api,octos-cli/telegram"
 *   # Start the server (tests assume it's running on OCTOS_TEST_URL or localhost:3000)
 *
 * Run:
 *   npx playwright test
 */
export default defineConfig({
  testDir: './tests',
  testMatch: [
    '**/*.spec.ts',
    '**/*.test.ts',
    '**/*.test.mjs',
    '**/*.property.ts',
  ],
  testIgnore: INCLUDE_LIVE_E2E ? [] : LIVE_E2E_TESTS,
  timeout: 60_000,
  retries: 0,
  use: {
    baseURL: process.env.OCTOS_TEST_URL || 'http://localhost:3000',
  },
});
