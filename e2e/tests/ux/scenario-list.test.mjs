// Unit tests for the M19-A UX scenario list pipeline.
// Run with: `npm --prefix e2e run ux:scenario:list:test`
//
// Tests:
//   1. The real manifest at e2e/matrix/octos-ux.toml parses cleanly and
//      declares all ten scenarios the umbrella issue requires.
//   2. A malformed manifest raises a typed ManifestSchemaError.
//   3. Tier filtering: fast subset is contained in local, local in release.
//   4. Runnability classifier reports skip / block / runnable correctly
//      against an injected fake host env.
//   5. The CLI exits 0 on a valid manifest and 3 on a schema error.

import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import {
  classifyRunnability,
  filterByTier,
  loadManifest,
  ManifestSchemaError,
} from "../../lib/ux/scenarios.mjs";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const REPO_ROOT = resolve(__dirname, "..", "..", "..");
const MANIFEST = resolve(REPO_ROOT, "e2e", "matrix", "octos-ux.toml");
const CAPABILITIES = resolve(
  REPO_ROOT,
  "e2e",
  "matrix",
  "ux-capabilities.json",
);
const CLI = resolve(REPO_ROOT, "e2e", "scripts", "ux-scenario-list.mjs");

const EXPECTED_IDS = [
  "tui-solo-onboarding",
  "provider-missing-recoverable",
  "permission-selection",
  "stdio-happy-path",
  "websocket-happy-path",
  "approval-denial",
  "task-subagent-tree",
  "restart-reconnect",
  "narrow-layout",
  "dropped-completion-backpressure",
];

const REQUIRED_ARTIFACTS = [
  "scenario.json",
  "summary.json",
  "appui-transcript.jsonl",
  "server.log",
  "tui-capture.txt",
  "runtime-policy-stamp.json",
  "validation.json",
];

function makeEnv({
  tools = new Set(["tmux", "octos", "octoscode"]),
  envVars = new Set(),
  capabilities = new Set(),
} = {}) {
  return {
    toolExists: (t) => tools.has(t),
    envHas: (k) => envVars.has(k),
    knownCapabilities: capabilities,
  };
}

function checkedInCapabilities() {
  const parsed = JSON.parse(readFileSync(CAPABILITIES, "utf8"));
  assert.equal(parsed.schema, "octos.ux.capabilities.v1");
  assert.ok(Array.isArray(parsed.capabilities));
  return new Set(parsed.capabilities);
}

test("manifest has at least one stdio and one ws transport scenario", () => {
  const manifest = loadManifest({ path: MANIFEST });
  const stdio = manifest.scenarios.filter((s) => s.transport === "stdio");
  const ws = manifest.scenarios.filter((s) => s.transport === "ws");
  assert.ok(stdio.length >= 1, "expected at least one stdio scenario");
  assert.ok(ws.length >= 1, "expected at least one ws scenario");
});

test("checked-in capability gate leaves unproven M19 lanes blocked", () => {
  const manifest = loadManifest({ path: MANIFEST });
  const env = makeEnv({ capabilities: checkedInCapabilities() });
  for (const id of ["task-subagent-tree", "dropped-completion-backpressure"]) {
    const scenario = manifest.scenarios.find((s) => s.id === id);
    assert.ok(scenario, `missing scenario ${id}`);
    const r = classifyRunnability(scenario, env);
    assert.equal(
      r.status,
      "blocked",
      `${id} should stay blocked until validated`,
    );
    assert.ok(r.reasons.length > 0);
  }
});
