import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { cpSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const REPO_ROOT = resolve(__dirname, "..", "..", "..");
const VALIDATOR = resolve(REPO_ROOT, "e2e", "scripts", "ux-tmux-validate.mjs");
const FIXTURE = resolve(REPO_ROOT, "e2e", "fixtures", "ux-artifact-self-test");

function copyFixture(name) {
  const root = mkdtempSync(join(tmpdir(), `octos-ux-validator-${name}-`));
  const artifactDir = join(root, "artifact");
  cpSync(FIXTURE, artifactDir, { recursive: true });
  return artifactDir;
}

function readJson(file) {
  return JSON.parse(readFileSync(file, "utf8"));
}

function writeJson(file, value) {
  writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
}

function runValidatorOk(artifactDir) {
  const out = execFileSync(process.execPath, [VALIDATOR, artifactDir], {
    cwd: REPO_ROOT,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
  return JSON.parse(out);
}

function runValidatorFail(artifactDir) {
  const result = spawnSync(process.execPath, [VALIDATOR, artifactDir], {
    cwd: REPO_ROOT,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
  assert.notEqual(result.status, 0, result.stdout || result.stderr);
  return JSON.parse(result.stdout);
}

function failureIds(result) {
  return result.failures.map((failure) => failure.id);
}

test("known-good UX artifact fixture passes named validator registry", () => {
  const artifactDir = copyFixture("good");
  writeFileSync(
    join(artifactDir, "tmux-cursor-samples.jsonl"),
    '{"ts":"2026-05-18T00:00:00.000Z","row":10,"col":2}\n',
  );
  const result = runValidatorOk(artifactDir);

  assert.equal(result.status, "passed");
  for (const id of [
    "artifact_abi",
    "appui_transcript_parseable",
    "capabilities_before_followups",
    "no_unadvertised_methods_called",
    "rendered_screen_no_known_bug_patterns",
    "terminal_layout_snapshot",
    "final_answer_visible",
    "composer_usable",
    "secret_redaction",
  ]) {
    assert.ok(result.validators.includes(id), `registry missing ${id}`);
    assert.ok(result.checks.some((check) => check.id === id), `checks missing ${id}`);
  }
  assert.equal(result.layout_snapshot.schema, "octos.ux.terminal_layout_snapshot.v1");
  assert.equal(result.layout_snapshot.status, "captured");
  assert.equal(result.layout_snapshot.cursor_samples.count, 1);
  const regions = new Set(result.layout_snapshot.regions.map((region) => region.id));
  assert.ok(regions.has("history"));
  assert.ok(regions.has("composer"));
  assert.ok(regions.has("status"));
});

test("declared final marker must be visible in the terminal capture", () => {
  const artifactDir = copyFixture("hidden-final");
  const scenarioPath = join(artifactDir, "scenario.json");
  const scenario = readJson(scenarioPath);
  scenario.final_marker = "NEVER_VISIBLE_FINAL_MARKER";
  scenario.acceptance = ["final_answer_visible"];
  writeJson(scenarioPath, scenario);
  writeFileSync(
    join(artifactDir, "tui-capture.txt"),
    [
      "› please finish with NEVER_VISIBLE_FINAL_MARKER",
      "",
      "Messages",
      "User: prompt echo only",
      "",
      "Composer",
      ">",
      "",
      "state Ready",
      "",
    ].join("\n"),
  );

  const result = runValidatorFail(artifactDir);
  assert.ok(failureIds(result).includes("final_answer_visible"));
});

test("unsupported AppUI method errors fail the advertised-method validator", () => {
  const artifactDir = copyFixture("unsupported-method");
  writeFileSync(
    join(artifactDir, "appui-transcript.jsonl"),
    `${readFileSync(join(artifactDir, "appui-transcript.jsonl"), "utf8")}` +
      '{"ts":"2026-05-18T00:00:00.050Z","direction":"server_to_client","frame":{"jsonrpc":"2.0","id":"bad-1","error":{"code":-32601,"message":"Method not found"}}}\n',
  );

  const result = runValidatorFail(artifactDir);
  assert.ok(failureIds(result).includes("no_unadvertised_methods_called"));
});

test("known bad terminal captures fail rendered-screen validator", () => {
  const artifactDir = copyFixture("bad-capture");
  writeFileSync(
    join(artifactDir, "tui-capture.txt"),
    [
      "┌Work › input overlapped a removed pane border",
      "• #### raw markdown control text leaked",
      " state ◐ duplicate ◑ Working",
      "",
      "Composer",
      ">",
      "",
    ].join("\n"),
  );

  const result = runValidatorFail(artifactDir);
  assert.ok(failureIds(result).includes("rendered_screen_no_known_bug_patterns"));
});

test("retained artifacts must not include raw secret-shaped values", () => {
  const artifactDir = copyFixture("secret-leak");
  const leaked = "sk-testvalidatorsecretleak0123456789";
  writeFileSync(join(artifactDir, "server.log"), `OPENAI_API_KEY=${leaked}\n`);

  const result = runValidatorFail(artifactDir);
  assert.ok(failureIds(result).includes("secret_redaction"));
  assert.doesNotMatch(
    result.failures.find((failure) => failure.id === "secret_redaction").detail,
    new RegExp(leaked),
  );
});
