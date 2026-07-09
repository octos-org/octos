import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const REPO_ROOT = resolve(__dirname, "..", "..", "..");
const CLI = resolve(REPO_ROOT, "e2e", "scripts", "ux-gate-report.mjs");

function runReport(args, options = {}) {
  return execFileSync(process.execPath, [CLI, ...args], {
    cwd: options.cwd ?? REPO_ROOT,
    encoding: "utf8",
  });
}

function readSummary(outDir) {
  return JSON.parse(readFileSync(join(outDir, "ux-summary.json"), "utf8"));
}

test("report writes JSON and Markdown summaries without counting skipped or blocked as passed", () => {
  const outDir = mkdtempSync(join(tmpdir(), "ux-gate-report-"));

  const stdout = runReport(["--tier", "fast", "--out-dir", outDir]);

  assert.match(stdout, /ux-summary\.json/);
  const summary = readSummary(outDir);
  assert.equal(summary.schema, "octos.ux.gate_summary.v1");
  assert.equal(summary.tier, "fast");
  assert.ok(summary.scenarios.length >= 1);
  assert.equal(
    summary.summary.total,
    summary.summary.passed
      + summary.summary.failed
      + summary.summary.skipped
      + summary.summary.blocked
      + summary.summary.quarantined
      + summary.summary.runnable
      + summary.summary.unknown,
  );
  assert.equal(summary.summary.passed, 0);
  assert.ok(
    summary.summary.skipped + summary.summary.blocked + summary.summary.runnable >= 1,
    "expected at least one non-passed scenario in a manifest-only report",
  );
  const first = summary.scenarios[0];
  assert.equal(typeof first.id, "string");
  assert.equal(typeof first.command, "string");
  assert.ok(Array.isArray(first.validators));
  assert.ok(Object.prototype.hasOwnProperty.call(first, "duration_ms"));
  assert.ok(Object.prototype.hasOwnProperty.call(first, "artifact_dir"));
  assert.ok(readFileSync(join(outDir, "ux-summary.md"), "utf8").includes("| Scenario |"));
});

test("report overlays artifact validation status and first actionable failure", () => {
  const root = mkdtempSync(join(tmpdir(), "ux-gate-artifact-"));
  const artifactDir = join(root, "run-1", "stdio-happy-path");
  const outDir = join(root, "summary");
  mkdirSync(artifactDir, { recursive: true });
  writeFileSync(
    join(artifactDir, "scenario.json"),
    JSON.stringify({ schema: "octos.ux.scenario.v1", id: "stdio-happy-path" }),
  );
  writeFileSync(
    join(artifactDir, "summary.json"),
    JSON.stringify({
      schema: "octos.ux.summary.v1",
      status: "failed",
      duration_ms: 1234,
    }),
  );
  writeFileSync(
    join(artifactDir, "validation.json"),
    JSON.stringify({
      schema: "octos.ux.validation.v1",
      status: "failed",
      checks: [
        {
          id: "artifact_abi",
          status: "passed",
          detail: "ok",
          evidence: ["scenario.json"],
        },
        {
          id: "composer_usable",
          status: "failed",
          detail: "composer hidden behind status row",
          evidence: ["tui-capture.txt"],
        },
      ],
      failures: [
        {
          id: "composer_usable",
          detail: "composer hidden behind status row",
          evidence: ["tui-capture.txt"],
        },
      ],
    }),
  );
  writeFileSync(
    join(artifactDir, "launch-command.txt"),
    "npm --prefix e2e run ux:tmux:run -- stdio-happy-path\n",
  );

  runReport([
    "--tier",
    "release",
    "--artifact-dir",
    artifactDir,
    "--out-dir",
    outDir,
  ]);

  const summary = readSummary(outDir);
  const row = summary.scenarios.find((entry) => entry.id === "stdio-happy-path");
  assert.equal(row.status, "failed");
  assert.equal(row.duration_ms, 1234);
  assert.equal(row.first_failure.id, "composer_usable");
  assert.match(row.first_failure.detail, /composer hidden/);
  assert.ok(row.validators.some((check) => check.id === "composer_usable"));
  assert.equal(summary.summary.failed, 1);
});

test("strict mode exits nonzero when any selected scenario is not passed", () => {
  const outDir = mkdtempSync(join(tmpdir(), "ux-gate-strict-"));

  assert.throws(
    () => runReport(["--tier", "fast", "--out-dir", outDir, "--strict"]),
    (error) => {
      assert.equal(error.status, 1);
      return true;
    },
  );
});

test("relative output paths resolve from repo root when invoked through e2e cwd", () => {
  const outDir = "e2e/test-results-ux/unit-relative-report";

  runReport(["--tier", "fast", "--out-dir", outDir], {
    cwd: resolve(REPO_ROOT, "e2e"),
  });

  const summary = readSummary(resolve(REPO_ROOT, outDir));
  assert.equal(summary.schema, "octos.ux.gate_summary.v1");
});
