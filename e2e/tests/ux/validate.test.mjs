import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const REPO_ROOT = resolve(__dirname, "..", "..", "..");
const VALIDATOR = resolve(REPO_ROOT, "e2e", "scripts", "ux-tmux-validate.mjs");

const REQUIRED_ARTIFACTS = [
  "scenario.json",
  "summary.json",
  "launch-command.txt",
  "terminal-size.json",
  "input-replay.log",
  "appui-transcript.jsonl",
  "server.log",
  "tui-capture.txt",
  "runtime-policy-stamp.json",
  "validation.json",
];

function writeJson(file, value) {
  writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`, "utf8");
}

function baseTranscript({
  advertiseBeforeFollowups = true,
  includeUnadvertised = false,
} = {}) {
  const rows = [];
  const helloTx = {
    direction: "client_to_server",
    frame: {
      jsonrpc: "2.0",
      id: "hello-1",
      method: "client_hello",
      params: { client: "validator-test" },
    },
  };
  const helloRx = {
    direction: "server_to_client",
    frame: {
      jsonrpc: "2.0",
      id: "hello-1",
      result: {
        capabilities: ["runtime.policy_stamp.v1"],
        supported_methods: ["session/open", "turn/start"],
      },
    },
  };
  const openTx = {
    direction: "client_to_server",
    frame: {
      jsonrpc: "2.0",
      id: "open-1",
      method: "session/open",
      params: { session_id: "ux-validator-test" },
    },
  };
  if (advertiseBeforeFollowups) {
    rows.push(helloTx, helloRx, openTx);
  } else {
    rows.push(helloTx, openTx, helloRx);
  }
  rows.push({
    direction: "server_to_client",
    frame: {
      jsonrpc: "2.0",
      id: "open-1",
      result: { opened: { session_id: "ux-validator-test" } },
    },
  });
  rows.push({
    direction: "client_to_server",
    frame: {
      jsonrpc: "2.0",
      id: "turn-1",
      method: includeUnadvertised ? "tool/status/list" : "turn/start",
      params: { session_id: "ux-validator-test" },
    },
  });
  rows.push({
    direction: "server_to_client",
    frame: {
      jsonrpc: "2.0",
      id: "turn-1",
      result: { ok: true },
    },
  });
  rows.push({
    direction: "server_to_client",
    frame: {
      jsonrpc: "2.0",
      method: "turn/completed",
      params: {
        session_id: "ux-validator-test",
        turn_id: "turn-1",
        status: "completed",
      },
    },
  });
  return rows.map((row) => JSON.stringify(row)).join("\n") + "\n";
}

function makeArtifactDir(overrides = {}) {
  const dir = mkdtempSync(join(tmpdir(), "octos-ux-validate-"));
  mkdirSync(dir, { recursive: true });
  writeJson(join(dir, "scenario.json"), {
    schema: "octos.ux.scenario.v1",
    id: "validator-test",
    artifact_abi: "octos.ux.artifacts.v1",
    final_marker: "M19_VALIDATOR_FINAL_LINE",
    required_artifacts: REQUIRED_ARTIFACTS,
    ...(overrides.scenario || {}),
  });
  writeJson(join(dir, "summary.json"), {
    schema: "octos.ux.summary.v1",
    status: "passed",
    mode: "self-test",
    scenario_id: "validator-test",
    artifacts: Object.fromEntries(
      REQUIRED_ARTIFACTS.map((name) => [name, name]),
    ),
    ...(overrides.summary || {}),
  });
  writeJson(join(dir, "terminal-size.json"), {
    schema: "octos.ux.terminal_size.v1",
    cols: 120,
    rows: 40,
    ...(overrides.terminal || {}),
  });
  writeJson(join(dir, "runtime-policy-stamp.json"), {
    schema: "octos.runtime_policy_stamp.v1",
    stamp: { sandbox_mode: "workspace-write", approval_policy: "never" },
  });
  writeFileSync(
    join(dir, "appui-transcript.jsonl"),
    overrides.transcript || baseTranscript(),
    "utf8",
  );
  writeFileSync(
    join(dir, "tui-capture.txt"),
    overrides.capture ||
      [
        "Octos TUI",
        "",
        "Messages",
        "User: validate the UX gate",
        "Assistant: completed M19_VALIDATOR_FINAL_LINE",
        "",
        "Composer",
        ">",
        "",
        "state Ready",
        "",
      ].join("\n"),
    "utf8",
  );
  writeFileSync(
    join(dir, "launch-command.txt"),
    "octoscode --mode protocol\n",
    "utf8",
  );
  writeFileSync(join(dir, "input-replay.log"), "line validate\n", "utf8");
  writeFileSync(
    join(dir, "server.log"),
    overrides.serverLog || "server ready\n",
    "utf8",
  );
  if (overrides.cursorSamples !== undefined) {
    writeFileSync(
      join(dir, "tmux-cursor-samples.jsonl"),
      overrides.cursorSamples,
      "utf8",
    );
  }
  writeJson(join(dir, "validation.json"), {
    schema: "octos.ux.validation.v1",
    status: "passed",
    checks: [],
  });
  return dir;
}

function runValidator(dir) {
  try {
    const stdout = execFileSync(process.execPath, [VALIDATOR, dir], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    });
    return { status: 0, result: JSON.parse(stdout) };
  } catch (error) {
    const stdout =
      error.stdout?.toString() ||
      readFileSync(join(dir, "validation.json"), "utf8");
    return { status: error.status, result: JSON.parse(stdout) };
  }
}

function checkById(result, id) {
  const check = result.checks.find((entry) => entry.id === id);
  assert.ok(check, `missing check ${id}`);
  return check;
}

test("unredacted retained secrets fail secret_redaction", () => {
  const dir = makeArtifactDir({
    serverLog: "OPENAI_API_KEY=sk-live-secret-1234567890 leaked\n",
  });
  const { status, result } = runValidator(dir);
  assert.equal(status, 1);
  assert.equal(checkById(result, "secret_redaction").status, "failed");
});

test("capability ordering and unadvertised method calls fail", () => {
  const dir = makeArtifactDir({
    transcript: baseTranscript({
      advertiseBeforeFollowups: false,
      includeUnadvertised: true,
    }),
  });
  const { status, result } = runValidator(dir);
  assert.equal(status, 1);
  assert.equal(
    checkById(result, "capabilities_before_followups").status,
    "failed",
  );
  assert.equal(
    checkById(result, "no_unadvertised_methods_called").status,
    "failed",
  );
});
