import { defineConfig } from '@playwright/test';

const LIVE_E2E_PATTERNS = [
  '**/live-*.spec.ts',
  '**/*-live.spec.ts',
  '**/mini*.spec.ts',
  '**/*mini*.spec.ts',
  '**/fleet-*.spec.ts',
  '**/*fleet*.spec.ts',
  '**/refactor-capabilities.spec.ts',
  '**/runtime-regression.spec.ts',
];

function hasExplicitTestSelection(argv: string[]): boolean {
  return argv.slice(2).some((arg) => {
    if (arg.startsWith('-')) return false;
    return (
      arg.includes('tests/') ||
      arg.includes('.spec.') ||
      arg.includes('.test.') ||
      arg.includes('.property.')
    );
  });
}

const includeLiveE2e =
  process.env.OCTOS_PLAYWRIGHT_LIVE === '1' ||
  hasExplicitTestSelection(process.argv);

/**
 * E2E tests for the octos web client + API.
 *
 * Prerequisites:
 *   cargo build --release -p octos-cli --features "octos-cli/api,octos-cli/telegram"
 *   # Start the server (tests assume it's running on OCTOS_TEST_URL or localhost:3000)
 *
 * Run:
 *   npx playwright test
 *
 * Default discovery excludes live/fleet/mini suites so a normal e2e run cannot
 * accidentally hit production hosts. Pass an explicit test path/glob, or set
 * OCTOS_PLAYWRIGHT_LIVE=1, for intentional live validation.
 */
export default defineConfig({
  testDir: './tests',
  testMatch: [
    '**/*.spec.ts',
    '**/*.test.ts',
    '**/*.test.mjs',
    '**/*.property.ts',
  ],
  testIgnore: includeLiveE2e ? [] : LIVE_E2E_PATTERNS,
  timeout: 60_000,
  retries: 0,
  use: {
    baseURL: process.env.OCTOS_TEST_URL || 'http://localhost:3000',
  },
});
