#!/usr/bin/env node
// M19-F: UX gate report
//
// Builds a run-level JSON + Markdown summary for the UX scenario gate. The
// report can be produced from manifest classification alone, or enriched with
// one or more real `ux:tmux:run` artifact directories.

import { execSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  classifyRunnability,
  filterByTier,
  loadManifest,
  ManifestSchemaError,
  TIER_ORDER,
} from "../lib/ux/scenarios.mjs";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const REPO_ROOT = resolve(__dirname, "..", "..");
const DEFAULT_MANIFEST = resolve(REPO_ROOT, "e2e", "matrix", "octos-ux.toml");
const DEFAULT_OUT_ROOT = resolve(REPO_ROOT, "e2e", "test-results-ux", "summaries");
const SUMMARY_JSON = "ux-summary.json";
const SUMMARY_MD = "ux-summary.md";
const VALID_STATUSES = new Set([
  "passed",
  "failed",
  "skipped",
  "blocked",
  "quarantined",
  "runnable",
  "unknown",
]);

class UsageError extends Error {}

function resolveRepoPath(value) {
  return resolve(REPO_ROOT, value);
}

function parseArgs(argv) {
  const opts = {
    tier: "local",
    manifest: DEFAULT_MANIFEST,
    outDir: null,
    artifactDirs: [],
    artifactRoot: null,
    strict: false,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--tier") {
      opts.tier = argv[++i];
    } else if (arg.startsWith("--tier=")) {
      opts.tier = arg.slice("--tier=".length);
    } else if (arg === "--manifest") {
      opts.manifest = resolveRepoPath(argv[++i]);
    } else if (arg.startsWith("--manifest=")) {
      opts.manifest = resolveRepoPath(arg.slice("--manifest=".length));
    } else if (arg === "--out-dir") {
      opts.outDir = resolveRepoPath(argv[++i]);
    } else if (arg.startsWith("--out-dir=")) {
      opts.outDir = resolveRepoPath(arg.slice("--out-dir=".length));
    } else if (arg === "--artifact-dir") {
      opts.artifactDirs.push(resolveRepoPath(argv[++i]));
    } else if (arg.startsWith("--artifact-dir=")) {
      opts.artifactDirs.push(resolveRepoPath(arg.slice("--artifact-dir=".length)));
    } else if (arg === "--artifact-root") {
      opts.artifactRoot = resolveRepoPath(argv[++i]);
    } else if (arg.startsWith("--artifact-root=")) {
      opts.artifactRoot = resolveRepoPath(arg.slice("--artifact-root=".length));
    } else if (arg === "--strict") {
      opts.strict = true;
    } else if (arg === "--help" || arg === "-h") {
      opts.help = true;
    } else {
      throw new UsageError(`unknown argument: ${arg}`);
    }
  }
  if (!opts.help && !TIER_ORDER.includes(opts.tier)) {
    throw new UsageError(
      `--tier must be one of ${TIER_ORDER.join(", ")} (got "${opts.tier}")`,
    );
  }
  return opts;
}

function usage() {
  return [
    "Usage: ux:gate:report [--tier fast|local|release] [--out-dir dir]",
    "                      [--artifact-dir dir ...] [--artifact-root dir]",
    "                      [--manifest path] [--strict]",
    "",
    "Writes ux-summary.json and ux-summary.md. Without artifact dirs, the",
    "summary reports manifest runnability. With artifact dirs, matching rows",
    "include real status, validators, duration, command, and first failure.",
  ].join("\n");
}

function compactTimestamp(date = new Date()) {
  return date.toISOString().replace(/[-:]/g, "").replace(/\.\d{3}Z$/, "Z");
}

function shellEscape(s) {
  return `'${s.replace(/'/g, "'\\''")}'`;
}

function makeEnv() {
  const knownCapabilities = new Set();
  const capsPath = resolve(REPO_ROOT, "e2e", "matrix", "ux-capabilities.json");
  if (existsSync(capsPath)) {
    try {
      const parsed = JSON.parse(readFileSync(capsPath, "utf8"));
      if (Array.isArray(parsed.capabilities)) {
        for (const cap of parsed.capabilities) {
          if (typeof cap === "string") knownCapabilities.add(cap);
        }
      }
    } catch {
      // Best-effort; absent or malformed capability data makes scenarios
      // blocked rather than falsely passed.
    }
  }
  return {
    toolExists(name) {
      try {
        execSync(`command -v ${shellEscape(name)}`, { stdio: "ignore" });
        return true;
      } catch {
        return false;
      }
    },
    envHas(name) {
      return typeof process.env[name] === "string" && process.env[name].length > 0;
    },
    knownCapabilities,
  };
}

function readJsonMaybe(file) {
  try {
    return { ok: true, value: JSON.parse(readFileSync(file, "utf8")) };
  } catch (error) {
    return { ok: false, error: String(error?.message ?? error) };
  }
}

function readTextMaybe(file) {
  try {
    return readFileSync(file, "utf8").trim();
  } catch {
    return null;
  }
}

function findArtifactDirs(root) {
  if (!root || !existsSync(root)) return [];
  const found = [];
  const stack = [root];
  while (stack.length > 0) {
    const dir = stack.pop();
    if (!dir) continue;
    if (existsSync(join(dir, "scenario.json")) && existsSync(join(dir, "summary.json"))) {
      found.push(dir);
      continue;
    }
    for (const entry of readdirSync(dir)) {
      const next = join(dir, entry);
      try {
        if (statSync(next).isDirectory()) stack.push(next);
      } catch {
        // Ignore concurrently removed dirs.
      }
    }
  }
  return found.sort();
}

function normalizeStatus(value) {
  if (typeof value !== "string") return "unknown";
  return VALID_STATUSES.has(value) ? value : "unknown";
}

function durationFrom(...objects) {
  for (const obj of objects) {
    if (!obj || typeof obj !== "object") continue;
    if (Number.isFinite(obj.duration_ms)) return obj.duration_ms;
    if (Number.isFinite(obj.durationMs)) return obj.durationMs;
    const started = typeof obj.started_at === "string" ? Date.parse(obj.started_at) : NaN;
    const finished = typeof obj.finished_at === "string" ? Date.parse(obj.finished_at) : NaN;
    if (Number.isFinite(started) && Number.isFinite(finished) && finished >= started) {
      return finished - started;
    }
  }
  return null;
}

function firstFailure(validation) {
  if (!validation || typeof validation !== "object") return null;
  if (Array.isArray(validation.failures) && validation.failures.length > 0) {
    const failure = validation.failures[0];
    return {
      id: String(failure.id ?? "validation_failure"),
      detail: String(failure.detail ?? "validator failed"),
      evidence: Array.isArray(failure.evidence) ? failure.evidence : [],
    };
  }
  if (Array.isArray(validation.checks)) {
    const check = validation.checks.find((entry) => entry?.status === "failed");
    if (check) {
      return {
        id: String(check.id ?? "validation_failure"),
        detail: String(check.detail ?? "validator failed"),
        evidence: Array.isArray(check.evidence) ? check.evidence : [],
      };
    }
  }
  return null;
}

function artifactRecord(dir) {
  const scenario = readJsonMaybe(join(dir, "scenario.json"));
  const summary = readJsonMaybe(join(dir, "summary.json"));
  const validation = readJsonMaybe(join(dir, "validation.json"));
  const soakSummary = readJsonMaybe(join(dir, "soak-summary.json"));
  const scenarioId = scenario.ok
    ? scenario.value.id ?? scenario.value.scenario_id
    : null;
  if (typeof scenarioId !== "string" || scenarioId.length === 0) return null;

  const checks = validation.ok && Array.isArray(validation.value.checks)
    ? validation.value.checks.map((check) => ({
        id: String(check.id ?? "unknown"),
        status: normalizeStatus(check.status),
        detail: typeof check.detail === "string" ? check.detail : "",
        evidence: Array.isArray(check.evidence) ? check.evidence : [],
      }))
    : [];
  const status = normalizeStatus(summary.ok ? summary.value.status : null);
  const validationStatus = normalizeStatus(validation.ok ? validation.value.status : null);
  return {
    id: scenarioId,
    artifact_dir: dir,
    status: validationStatus === "failed" ? "failed" : status,
    command: readTextMaybe(join(dir, "launch-command.txt")),
    duration_ms: durationFrom(
      summary.ok ? summary.value : null,
      soakSummary.ok ? soakSummary.value : null,
    ),
    validators: checks,
    first_failure: validation.ok ? firstFailure(validation.value) : {
      id: "validation_json",
      detail: validation.error,
      evidence: ["validation.json"],
    },
  };
}

function baseRow(scenario, classification) {
  return {
    id: scenario.id,
    title: scenario.title,
    tier: scenario.tier,
    transport: scenario.transport,
    provider: scenario.provider,
    terminal: scenario.terminal,
    status: classification.status,
    reasons: classification.reasons,
    command: `npm --prefix e2e run ux:tmux:run -- ${scenario.id}`,
    duration_ms: null,
    validators: scenario.acceptance.map((id) => ({
      id,
      status: "not_run",
      detail: "declared by manifest",
      evidence: [],
    })),
    artifact_dir: null,
    first_failure: classification.reasons.length > 0
      ? {
          id: `${classification.status}_reason`,
          detail: classification.reasons[0],
          evidence: ["e2e/matrix/octos-ux.toml"],
        }
      : null,
  };
}

function summarize(rows) {
  const counts = {
    total: rows.length,
    passed: 0,
    failed: 0,
    skipped: 0,
    blocked: 0,
    quarantined: 0,
    runnable: 0,
    unknown: 0,
  };
  for (const row of rows) {
    const key = VALID_STATUSES.has(row.status) ? row.status : "unknown";
    counts[key] += 1;
  }
  return counts;
}

function mdEscape(value) {
  return String(value ?? "")
    .replace(/\|/g, "\\|")
    .replace(/\n/g, " ");
}

function formatDuration(ms) {
  if (!Number.isFinite(ms)) return "";
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

function formatRelative(fileOrDir) {
  if (!fileOrDir) return "";
  const rel = relative(REPO_ROOT, fileOrDir);
  return rel && !rel.startsWith("..") ? rel : fileOrDir;
}

function renderMarkdown(report) {
  const lines = [
    `# UX Gate Summary`,
    "",
    `Generated: ${report.generated_at}`,
    `Tier: ${report.tier}`,
    "",
    `Passed: ${report.summary.passed} | Failed: ${report.summary.failed} | Skipped: ${report.summary.skipped} | Blocked: ${report.summary.blocked} | Quarantined: ${report.summary.quarantined} | Runnable-not-run: ${report.summary.runnable}`,
    "",
    "| Scenario | Tier | Transport | Status | Duration | Artifact | Command | First failure |",
    "|---|---:|---|---|---:|---|---|---|",
  ];
  for (const row of report.scenarios) {
    lines.push(
      [
        mdEscape(row.id),
        mdEscape(row.tier),
        mdEscape(row.transport),
        mdEscape(row.status),
        mdEscape(formatDuration(row.duration_ms)),
        mdEscape(formatRelative(row.artifact_dir)),
        mdEscape(row.command),
        mdEscape(row.first_failure?.detail ?? ""),
      ].join(" | ").replace(/^/, "| ").replace(/$/, " |"),
    );
  }
  lines.push("");
  return lines.join("\n");
}

function main() {
  let opts;
  try {
    opts = parseArgs(process.argv.slice(2));
  } catch (error) {
    if (error instanceof UsageError) {
      process.stderr.write(`error: ${error.message}\n\n${usage()}\n`);
      process.exit(2);
    }
    throw error;
  }
  if (opts.help) {
    process.stdout.write(`${usage()}\n`);
    return;
  }

  let manifest;
  try {
    manifest = loadManifest({ path: opts.manifest });
  } catch (error) {
    if (error instanceof ManifestSchemaError) {
      process.stderr.write(`manifest schema error: ${error.message}\n`);
      process.exit(3);
    }
    throw error;
  }

  const env = makeEnv();
  const artifactDirs = [
    ...opts.artifactDirs,
    ...findArtifactDirs(opts.artifactRoot),
  ];
  const artifactByScenario = new Map();
  for (const dir of artifactDirs) {
    const record = artifactRecord(dir);
    if (record) artifactByScenario.set(record.id, record);
  }

  const scenarios = filterByTier(manifest.scenarios, opts.tier)
    .sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
  const rows = scenarios.map((scenario) => {
    const row = baseRow(scenario, classifyRunnability(scenario, env));
    const artifact = artifactByScenario.get(scenario.id);
    if (!artifact) return row;
    return {
      ...row,
      status: artifact.status,
      command: artifact.command || row.command,
      duration_ms: artifact.duration_ms,
      validators: artifact.validators.length > 0 ? artifact.validators : row.validators,
      artifact_dir: artifact.artifact_dir,
      first_failure: artifact.first_failure,
      reasons: artifact.first_failure ? [artifact.first_failure.detail] : [],
    };
  });

  const outDir = opts.outDir || resolve(DEFAULT_OUT_ROOT, `ux-gate-${compactTimestamp()}`);
  mkdirSync(outDir, { recursive: true });
  const report = {
    schema: "octos.ux.gate_summary.v1",
    generated_at: new Date().toISOString(),
    tier: opts.tier,
    manifest: formatRelative(opts.manifest),
    summary: summarize(rows),
    scenarios: rows,
  };
  const jsonPath = join(outDir, SUMMARY_JSON);
  const mdPath = join(outDir, SUMMARY_MD);
  writeFileSync(jsonPath, `${JSON.stringify(report, null, 2)}\n`);
  writeFileSync(mdPath, renderMarkdown(report));
  process.stdout.write(`UX gate summary: ${jsonPath}\n`);
  process.stdout.write(`UX gate markdown: ${mdPath}\n`);

  if (opts.strict && report.summary.passed !== report.summary.total) {
    process.exit(1);
  }
}

main();
