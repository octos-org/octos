import { defineConfig } from '@playwright/test';

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
  timeout: 60_000,
  retries: 0,
  use: {
    baseURL: process.env.OCTOS_TEST_URL || 'http://localhost:3000',
  },
});
