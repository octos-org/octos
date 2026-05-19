#!/usr/bin/env node

import path from 'node:path';

const repoRoot = path.resolve(import.meta.dirname, '..', '..');
const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\..+/, 'Z');

process.env.OCTOS_M18_CODEX_P0_SOAK = '1';
process.env.OCTOS_M18_APPUI_PARITY_MODE ||= 'headless';
process.env.OCTOS_M18_APPUI_PARITY_DIR ||= path.join(
  repoRoot,
  'e2e',
  'test-results-m14-codex-tool-parity',
  stamp,
);

await import('./m18-appui-transport-parity-soak.mjs');
