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
  tools = new Set(["tmux", "octos", "octos-tui"]),
  envVars = new Set(),
  capabilities = new Set(),
} = {}) {
  return {
    toolExists: (t) => tools.has(t),
    envHas: (k) => envVars.has(k),
    knownCapabilities: capabilities,
  };
}

test("manifest parses and declares all umbrella-required scenarios", () => {
  const manifest = loadManifest({ path: MANIFEST });
  assert.equal(manifest.schemaVersion, 1);
  assert.equal(manifest.pack, "octos-ux");
  const ids = manifest.scenarios.map((s) => s.id).sort();
  assert.deepEqual(ids, [...EXPECTED_IDS].sort());
  for (const s of manifest.scenarios) {
    for (const required of REQUIRED_ARTIFACTS) {
      assert.ok(
        s.expectedArtifacts.includes(required),
        `scenario ${s.id} missing required artifact ${required}`,
      );
    }
    assert.ok(s.acceptance.length > 0, `scenario ${s.id} declares no validators`);
  }
});

test("manifest has at least one stdio and one ws transport scenario", () => {
  const manifest = loadManifest({ path: MANIFEST });
  const stdio = manifest.scenarios.filter((s) => s.transport === "stdio");
  const ws = manifest.scenarios.filter((s) => s.transport === "ws");
  assert.ok(stdio.length >= 1, "expected at least one stdio scenario");
  assert.ok(ws.length >= 1, "expected at least one ws scenario");
});

test("malformed manifest raises a typed ManifestSchemaError", () => {
  const dir = mkdtempSync(join(tmpdir(), "ux-manifest-"));
  const bad = join(dir, "octos-ux.toml");
  writeFileSync(
    bad,
    [
      'schema_version = 1',
      'pack = "octos-ux"',
      'owner = "test"',
      "[[scenario]]",
      'id = "missing-fields"',
      "",
    ].join("\n"),
  );
  assert.throws(() => loadManifest({ path: bad }), (err) => {
    assert.ok(err instanceof ManifestSchemaError);
    assert.match(err.message, /missing required field/);
    return true;
  });
});

test("unsupported schema_version raises typed error", () => {
  const dir = mkdtempSync(join(tmpdir(), "ux-manifest-"));
  const bad = join(dir, "octos-ux.toml");
  writeFileSync(
    bad,
    ['schema_version = 999', 'pack = "p"', 'owner = "o"', ""].join("\n"),
  );
  assert.throws(() => loadManifest({ path: bad }), (err) => {
    assert.ok(err instanceof ManifestSchemaError);
    assert.match(err.message, /unsupported schema_version/);
    return true;
  });
});

test("duplicate scenario id raises typed error", () => {
  const dir = mkdtempSync(join(tmpdir(), "ux-manifest-"));
  const bad = join(dir, "octos-ux.toml");
  const body = readFileSync(MANIFEST, "utf8");
  // Append a duplicate of the first scenario id to trigger duplicate detection.
  writeFileSync(
    bad,
    body +
      "\n[[scenario]]\n" +
      'id = "tui-solo-onboarding"\n' +
      'title = "dup"\n' +
      'description = "dup"\n' +
      'tier = "fast"\n' +
      'transport = "stdio"\n' +
      'provider = "fixture"\n' +
      'terminal = "80x24"\n' +
      'tui_binary = "octos-tui"\n' +
      'tmux_command = "x"\n' +
      "required_tools = []\n" +
      "required_capabilities = []\n" +
      "expected_artifacts = []\n" +
      "acceptance = []\n",
  );
  assert.throws(() => loadManifest({ path: bad }), (err) => {
    assert.ok(err instanceof ManifestSchemaError);
    assert.match(err.message, /duplicate scenario id/);
    return true;
  });
});

test("filterByTier honors fast / local / release ordering", () => {
  const manifest = loadManifest({ path: MANIFEST });
  const fast = filterByTier(manifest.scenarios, "fast");
  const local = filterByTier(manifest.scenarios, "local");
  const release = filterByTier(manifest.scenarios, "release");
  assert.ok(fast.length >= 1, "expected at least one fast-tier scenario");
  assert.ok(local.length >= fast.length, "local must be a superset of fast");
  assert.equal(
    release.length,
    manifest.scenarios.length,
    "release tier must include every scenario",
  );
  // Every fast scenario id must appear in local; every local id in release.
  const fastIds = new Set(fast.map((s) => s.id));
  const localIds = new Set(local.map((s) => s.id));
  const releaseIds = new Set(release.map((s) => s.id));
  for (const id of fastIds) assert.ok(localIds.has(id));
  for (const id of localIds) assert.ok(releaseIds.has(id));
});

test("filterByTier rejects an unknown tier", () => {
  const manifest = loadManifest({ path: MANIFEST });
  assert.throws(() => filterByTier(manifest.scenarios, "experimental"));
});

test("classifyRunnability reports runnable when all gates are green", () => {
  const manifest = loadManifest({ path: MANIFEST });
  const scenario = manifest.scenarios.find((s) => s.id === "stdio-happy-path");
  const env = makeEnv({
    capabilities: new Set(scenario.requiredCapabilities),
  });
  const r = classifyRunnability(scenario, env);
  assert.equal(r.status, "runnable");
  assert.deepEqual(r.reasons, []);
});

test("classifyRunnability reports skipped when host tool missing", () => {
  const manifest = loadManifest({ path: MANIFEST });
  const scenario = manifest.scenarios.find((s) => s.id === "stdio-happy-path");
  const env = makeEnv({
    tools: new Set(["octos", "octos-tui"]), // tmux missing
    capabilities: new Set(scenario.requiredCapabilities),
  });
  const r = classifyRunnability(scenario, env);
  assert.equal(r.status, "skipped");
  assert.ok(r.reasons.some((reason) => reason.includes("tmux")));
});

test("classifyRunnability reports blocked when capability missing", () => {
  const manifest = loadManifest({ path: MANIFEST });
  const scenario = manifest.scenarios.find((s) => s.id === "stdio-happy-path");
  const env = makeEnv({
    capabilities: new Set(), // no capabilities advertised
  });
  const r = classifyRunnability(scenario, env);
  assert.equal(r.status, "blocked");
  assert.ok(r.reasons.length > 0);
});

test("classifyRunnability honors quarantine flag", () => {
  const fakeScenario = {
    id: "q",
    requiredTools: [],
    requiredCapabilities: [],
    provider: "fixture",
    quarantine: true,
  };
  const r = classifyRunnability(fakeScenario, makeEnv());
  assert.equal(r.status, "quarantined");
});

test("CLI exits 0 on a valid manifest and JSON output round-trips", () => {
  const out = execFileSync(process.execPath, [CLI, "--tier", "fast", "--json"], {
    encoding: "utf8",
  });
  const parsed = JSON.parse(out);
  assert.equal(parsed.tier_filter, "fast");
  assert.equal(parsed.pack, "octos-ux");
  assert.ok(parsed.scenarios.length >= 1);
  for (const s of parsed.scenarios) {
    assert.ok(["runnable", "skipped", "blocked", "quarantined"].includes(s.status));
  }
});

test("CLI exits 2 on usage error", () => {
  try {
    execFileSync(process.execPath, [CLI, "--tier", "no-such-tier"], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    });
    assert.fail("expected non-zero exit");
  } catch (err) {
    assert.equal(err.status, 2);
  }
});

test("CLI exits 3 on a malformed manifest", () => {
  const dir = mkdtempSync(join(tmpdir(), "ux-cli-"));
  const bad = join(dir, "octos-ux.toml");
  writeFileSync(bad, 'schema_version = "not-an-int"\n');
  try {
    execFileSync(
      process.execPath,
      [CLI, "--manifest", bad, "--tier", "fast"],
      { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
    );
    assert.fail("expected non-zero exit");
  } catch (err) {
    assert.equal(err.status, 3);
  }
});

test("CLI plain output is deterministic for a fixed tier", () => {
  const first = execFileSync(process.execPath, [CLI, "--tier", "local"], {
    encoding: "utf8",
  });
  const second = execFileSync(process.execPath, [CLI, "--tier", "local"], {
    encoding: "utf8",
  });
  assert.equal(first, second);
});
