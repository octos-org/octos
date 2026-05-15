#!/usr/bin/env node
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import readline from "node:readline";
import { fileURLToPath } from "node:url";
import WebSocket from "../e2e/node_modules/ws/index.js";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

const FEATURE_TOKENS = [
  "approval.typed.v1",
  "pane.snapshots.v1",
  "session.workspace_cwd.v1",
  "harness.task_control.v1",
  "state.session_hydrate.v1",
  "state.thread_graph.v1",
  "state.turn_state_get.v1",
  "event.message_persisted.v1",
  "event.spawn_complete.v1",
  "auxiliary.rest_to_ws.v1",
];

const OTP_METHODS = new Set(["auth/send_code", "auth/verify"]);
const PERMISSION_MODES = {
  workspaceWrite: {
    mode: "workspace-write",
    network: "deny",
    approval_policy: "on-request",
    sandbox_mode: "workspace-write",
  },
  approvalNeverWorkspace: {
    mode: "workspace-write",
    network: "deny",
    approval_policy: "never",
    sandbox_mode: "workspace-write",
  },
  dangerFullAccess: {
    mode: "danger-full-access",
    network: "allow",
    approval_policy: "never",
    sandbox_mode: "danger-full-access",
  },
};

function parseArgs(argv) {
  const out = {
    transport: "fixture",
    outDir: "",
    endpoint: "ws://127.0.0.1:50179/api/ui-protocol/ws",
    token: "octos-m12-soak-token",
    stdioCommand: "",
    workspace: "",
    dataDir: "",
    profileId: "m12solo",
    sessionId: "",
    localName: "M12 Solo User",
    localUsername: "m12solo",
    localEmail: "m12solo@example.invalid",
    serverLog: "",
    strict: false,
    tenantNegative: true,
    requestTimeoutMs: 10_000,
    connectTimeoutMs: 10_000,
  };
  for (let i = 2; i < argv.length; i += 1) {
    const arg = argv[i];
    const next = () => {
      i += 1;
      if (i >= argv.length) throw new Error(`missing value after ${arg}`);
      return argv[i];
    };
    switch (arg) {
      case "--transport":
        out.transport = next();
        break;
      case "--out-dir":
        out.outDir = next();
        break;
      case "--endpoint":
        out.endpoint = next();
        break;
      case "--auth-token":
        out.token = next();
        break;
      case "--stdio-command":
        out.stdioCommand = next();
        break;
      case "--workspace":
        out.workspace = next();
        break;
      case "--data-dir":
        out.dataDir = next();
        break;
      case "--profile-id":
        out.profileId = next();
        break;
      case "--session-id":
        out.sessionId = next();
        break;
      case "--local-name":
        out.localName = next();
        break;
      case "--local-username":
        out.localUsername = next();
        break;
      case "--local-email":
        out.localEmail = next();
        break;
      case "--server-log":
        out.serverLog = next();
        break;
      case "--strict":
        out.strict = true;
        break;
      case "--no-tenant-negative":
        out.tenantNegative = false;
        break;
      case "--request-timeout-ms":
        out.requestTimeoutMs = Number(next());
        break;
      case "--connect-timeout-ms":
        out.connectTimeoutMs = Number(next());
        break;
      case "--help":
      case "-h":
        usage(0);
        break;
      default:
        throw new Error(`unknown argument: ${arg}`);
    }
  }
  if (!["fixture", "stdio", "ws"].includes(out.transport)) {
    throw new Error(`--transport must be fixture, stdio, or ws; got ${out.transport}`);
  }
  if (!out.outDir) {
    out.outDir = fs.mkdtempSync(path.join(os.tmpdir(), "octos-m12-solo-probe-"));
  }
  if (!out.workspace) {
    out.workspace = path.join(out.outDir, "workspace");
  }
  if (!out.dataDir) {
    out.dataDir = path.join(out.outDir, "data");
  }
  if (!out.sessionId) {
    out.sessionId = `${out.profileId}:local:m12-solo#${Date.now()}`;
  }
  return out;
}

function usage(code) {
  const stream = code === 0 ? process.stdout : process.stderr;
  stream.write(`Usage: node scripts/m12-solo-appui-probe.mjs [options]

Options:
  --transport <fixture|stdio|ws>       Transport to probe. Default: fixture.
  --out-dir <DIR>                      Artifact directory.
  --endpoint <WS_URL>                  WebSocket AppUI endpoint.
  --auth-token <TOKEN>                 WebSocket bearer token.
  --stdio-command <CMD>                Command for stdio transport.
  --workspace <DIR>                    Workspace cwd requested in session/open.
  --data-dir <DIR>                     Runtime data dir used by wrapper scripts.
  --profile-id <ID>                    Profile id/session profile. Default: m12solo.
  --session-id <ID>                    Session id. Default includes profile id.
  --local-name <NAME>                  profile/local/create name.
  --local-username <USERNAME>          profile/local/create username.
  --local-email <EMAIL>                profile/local/create email metadata.
  --server-log <FILE>                  Append stdio child stderr to this log.
  --strict                             Exit non-zero if M12 methods are blocked.
  --no-tenant-negative                 Skip tenant/cloud dangerous rejection probe.
`);
  process.exit(code);
}

function ensureDir(dir) {
  fs.mkdirSync(dir, { recursive: true });
}

function writeJson(file, value) {
  ensureDir(path.dirname(file));
  fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
}

function appendJsonl(file, value) {
  ensureDir(path.dirname(file));
  fs.appendFileSync(file, `${JSON.stringify(value)}\n`);
}

function nowIso() {
  return new Date().toISOString();
}

function normalizeWsUrl(raw) {
  let url = raw.replace(/^http:/, "ws:").replace(/^https:/, "wss:").replace(/\/$/, "");
  if (!url.includes("/api/ui-protocol/ws")) {
    url = `${url}/api/ui-protocol/ws`;
  }
  const join = url.includes("?") ? "&" : "?";
  return `${url}${join}${FEATURE_TOKENS.map((f) => `ui_feature=${encodeURIComponent(f)}`).join("&")}`;
}

function rpcErrorObject(error) {
  if (error instanceof RpcError) {
    return { code: error.code, message: error.message, data: error.data };
  }
  return { code: "transport", message: error?.message ?? String(error) };
}

function resultSummary(method, result) {
  if (method === "config/capabilities/list") {
    const capabilities = result?.capabilities ?? result ?? {};
    return {
      capabilities: {
        schema_version: capabilities.schema_version,
        supported_features: capabilities.supported_features ?? [],
        supported_methods_count: Array.isArray(capabilities.supported_methods)
          ? capabilities.supported_methods.length
          : undefined,
      },
    };
  }
  if (result?.opened?.capabilities) {
    const capabilities = result.opened.capabilities;
    return {
      ...result,
      opened: {
        ...result.opened,
        capabilities: {
          schema_version: capabilities.schema_version,
          supported_features: capabilities.supported_features ?? [],
          supported_methods_count: Array.isArray(capabilities.supported_methods)
            ? capabilities.supported_methods.length
            : undefined,
        },
      },
    };
  }
  if (result?.capabilities?.supported_methods) {
    return {
      ...result,
      capabilities: {
        ...result.capabilities,
        supported_methods: undefined,
        supported_methods_count: result.capabilities.supported_methods.length,
      },
    };
  }
  return result;
}

function notificationSummary(frame) {
  if (frame?.params?.capabilities?.supported_methods) {
    return {
      ...frame,
      params: {
        ...frame.params,
        capabilities: {
          ...frame.params.capabilities,
          supported_methods: undefined,
          supported_methods_count: frame.params.capabilities.supported_methods.length,
        },
      },
    };
  }
  return frame;
}

class RpcError extends Error {
  constructor(error) {
    super(error?.message ?? "RPC error");
    this.name = "RpcError";
    this.code = error?.code;
    this.data = error?.data;
  }
}

class Recorder {
  constructor(options) {
    this.options = options;
    this.paths = {
      transcript: path.join(options.outDir, "appui-transcript.jsonl"),
      approvalEvents: path.join(options.outDir, "approval-events.jsonl"),
      runtimePolicy: path.join(options.outDir, "runtime-policy-stamp.json"),
      toolRegistry: path.join(options.outDir, "tool-registry-snapshot.json"),
      filesystemProbe: path.join(options.outDir, "filesystem-probe.json"),
      summary: path.join(options.outDir, "soak-summary.json"),
    };
    this.sentMethods = [];
    this.receivedOtpMethodText = false;
    this.approvalEvents = [];
  }

  resetArtifacts() {
    ensureDir(this.options.outDir);
    for (const file of Object.values(this.paths)) {
      fs.rmSync(file, { force: true });
    }
    fs.writeFileSync(this.paths.transcript, "");
    fs.writeFileSync(this.paths.approvalEvents, "");
  }

  recordTx(id, method, params, caseName) {
    this.sentMethods.push(method);
    appendJsonl(this.paths.transcript, {
      ts: nowIso(),
      direction: "tx",
      id,
      method,
      case: caseName,
      params,
    });
  }

  recordRx(id, method, payload, caseName) {
    const entry = {
      ts: nowIso(),
      direction: "rx",
      id,
      method,
      case: caseName,
      ...payload,
    };
    appendJsonl(this.paths.transcript, entry);
    const text = JSON.stringify(entry);
    if (text.includes("auth/send_code") || text.includes("auth/verify")) {
      this.receivedOtpMethodText = true;
    }
  }

  recordNotification(frame) {
    const summary = notificationSummary(frame);
    appendJsonl(this.paths.transcript, {
      ts: nowIso(),
      direction: "rx",
      notification: true,
      ...summary,
    });
    if (String(frame.method ?? "").startsWith("approval/")) {
      this.approvalEvents.push(frame);
      appendJsonl(this.paths.approvalEvents, {
        ts: nowIso(),
        ...frame,
      });
    }
  }

  assertNoOtpTraffic() {
    const sentOtp = this.sentMethods.filter((method) => OTP_METHODS.has(method));
    const transcript = fs.existsSync(this.paths.transcript)
      ? fs.readFileSync(this.paths.transcript, "utf8")
      : "";
    const transcriptHasOtp = /auth\/(?:send_code|verify)/.test(transcript);
    return {
      ok: sentOtp.length === 0 && !transcriptHasOtp && !this.receivedOtpMethodText,
      sent_otp_methods: sentOtp,
      transcript_mentions_otp_methods: transcriptHasOtp || this.receivedOtpMethodText,
    };
  }
}

class WsTransport {
  constructor(options, recorder) {
    this.options = options;
    this.recorder = recorder;
    this.ws = undefined;
    this.pending = new Map();
  }

  async connect() {
    const url = normalizeWsUrl(this.options.endpoint);
    this.ws = new WebSocket(url, {
      headers: {
        Authorization: `Bearer ${this.options.token}`,
        "X-Octos-Ui-Features": FEATURE_TOKENS.join(","),
        "X-Profile-Id": this.options.profileId,
      },
    });
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        reject(new Error(`connect timeout after ${this.options.connectTimeoutMs}ms`));
      }, this.options.connectTimeoutMs);
      this.ws.once("open", () => {
        clearTimeout(timer);
        resolve();
      });
      this.ws.once("error", (error) => {
        clearTimeout(timer);
        reject(error);
      });
    });
    this.ws.on("message", (data) => this.handleMessage(data.toString()));
    this.ws.on("close", () => {
      for (const pending of this.pending.values()) {
        clearTimeout(pending.timer);
        pending.reject(new Error("WebSocket closed before response"));
      }
      this.pending.clear();
    });
  }

  handleMessage(text) {
    let frame;
    try {
      frame = JSON.parse(text);
    } catch {
      return;
    }
    if (frame && Object.prototype.hasOwnProperty.call(frame, "id")) {
      const id = String(frame.id);
      const pending = this.pending.get(id);
      if (!pending) return;
      this.pending.delete(id);
      clearTimeout(pending.timer);
      if (frame.error) {
        pending.reject(new RpcError(frame.error));
      } else {
        pending.resolve(frame.result);
      }
      return;
    }
    if (frame?.method) {
      this.recorder.recordNotification(frame);
    }
  }

  async request(id, method, params) {
    if (!this.ws) await this.connect();
    const frame = { jsonrpc: "2.0", id, method, params };
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`timeout waiting for ${method}`));
      }, this.options.requestTimeoutMs);
      this.pending.set(id, { resolve, reject, timer });
      this.ws.send(JSON.stringify(frame), (error) => {
        if (error) {
          clearTimeout(timer);
          this.pending.delete(id);
          reject(error);
        }
      });
    });
  }

  async close() {
    if (!this.ws) return;
    await new Promise((resolve) => {
      const ws = this.ws;
      ws.once("close", resolve);
      ws.close();
      setTimeout(resolve, 500);
    });
  }
}

class StdioTransport {
  constructor(options, recorder) {
    this.options = options;
    this.recorder = recorder;
    this.child = undefined;
    this.pending = new Map();
  }

  async connect() {
    if (!this.options.stdioCommand) {
      throw new Error("--stdio-command is required for stdio transport");
    }
    this.child = spawn(this.options.stdioCommand, {
      cwd: this.options.workspace,
      env: { ...process.env },
      shell: true,
      stdio: ["pipe", "pipe", "pipe"],
    });
    const rl = readline.createInterface({ input: this.child.stdout });
    rl.on("line", (line) => this.handleLine(line));
    this.child.stderr.on("data", (chunk) => {
      if (this.options.serverLog) {
        fs.appendFileSync(this.options.serverLog, chunk);
      }
    });
    this.child.on("exit", (code, signal) => {
      for (const pending of this.pending.values()) {
        clearTimeout(pending.timer);
        pending.reject(new Error(`stdio child exited before response: code=${code} signal=${signal}`));
      }
      this.pending.clear();
    });
    await new Promise((resolve) => setTimeout(resolve, 250));
  }

  handleLine(line) {
    let frame;
    try {
      frame = JSON.parse(line);
    } catch {
      return;
    }
    if (frame && Object.prototype.hasOwnProperty.call(frame, "id")) {
      const id = String(frame.id);
      const pending = this.pending.get(id);
      if (!pending) return;
      this.pending.delete(id);
      clearTimeout(pending.timer);
      if (frame.error) {
        pending.reject(new RpcError(frame.error));
      } else {
        pending.resolve(frame.result);
      }
      return;
    }
    if (frame?.method) {
      this.recorder.recordNotification(frame);
    }
  }

  async request(id, method, params) {
    if (!this.child) await this.connect();
    const frame = { jsonrpc: "2.0", id, method, params };
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`timeout waiting for ${method}`));
      }, this.options.requestTimeoutMs);
      this.pending.set(id, { resolve, reject, timer });
      this.child.stdin.write(`${JSON.stringify(frame)}\n`, (error) => {
        if (error) {
          clearTimeout(timer);
          this.pending.delete(id);
          reject(error);
        }
      });
    });
  }

  async close() {
    if (!this.child) return;
    this.child.kill("SIGTERM");
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
}

class FixtureTransport {
  constructor(options, recorder) {
    this.options = options;
    this.recorder = recorder;
    this.permission = { ...PERMISSION_MODES.workspaceWrite };
  }

  async connect() {}

  async request(_id, method, params) {
    switch (method) {
      case "config/capabilities/list":
        return {
          capabilities: {
            schema_version: 2,
            protocol: "octos-ui/v1alpha1",
            supported_features: FEATURE_TOKENS,
            supported_methods: [
              "config/capabilities/list",
              "profile/local/create",
              "session/open",
              "session/status/read",
              "permission/profile/list",
              "permission/profile/set",
              "tool/status/list",
              "turn/start",
            ],
          },
        };
      case "profile/local/create":
        return {
          profile_id: params.username,
          user_id: params.username,
          name: params.name,
          username: params.username,
          email: params.email,
          created: true,
          runtime_mode: "solo",
        };
      case "session/open":
        return {
          opened: {
            session_id: params.session_id,
            active_profile_id: params.profile_id,
            workspace_root: path.resolve(params.cwd),
            cursor: { stream: params.session_id, seq: 1 },
          },
        };
      case "session/status/read":
        return {
          session_id: params.session_id,
          profile_id: params.profile_id,
          runtime_policy_stamp: this.policyStamp(params),
          health: { status: "ok" },
        };
      case "permission/profile/list":
        return {
          session_id: params.session_id,
          current: { mode: this.permission.mode, network: this.permission.network },
          profiles: [
            { mode: "read-only", network: "deny" },
            { mode: "workspace-write", network: "deny" },
            { mode: "danger-full-access", network: "allow" },
          ],
        };
      case "permission/profile/set": {
        const update = params.update ?? {};
        if (params.runtime_mode === "tenant" && update.mode === "danger-full-access") {
          throw new RpcError({
            code: -32120,
            message: "danger-full-access is rejected outside local solo mode",
            data: { kind: "dangerous_mode_rejected", runtime_mode: "tenant" },
          });
        }
        this.permission = { ...this.permission, ...update };
        return {
          session_id: params.session_id,
          current: { mode: this.permission.mode, network: this.permission.network },
          applied: true,
        };
      }
      case "tool/status/list":
        return {
          session_id: params.session_id,
          policy_id: this.permission.mode === "danger-full-access" ? "allow-all" : "workspace",
          tools: [
            { name: "shell", enabled: true, approval_policy: this.permission.approval_policy },
            { name: "filesystem", enabled: true, scope: this.permission.mode },
          ],
        };
      case "turn/start":
        return { accepted: true };
      default:
        throw new RpcError({
          code: -32601,
          message: `method not found: ${method}`,
          data: { kind: "method_not_found", method },
        });
    }
  }

  policyStamp(params) {
    const mode = this.permission.mode;
    const danger = mode === "danger-full-access";
    return {
      runtime_mode: "solo",
      profile_id: params.profile_id,
      workspace_root: path.resolve(this.options.workspace),
      approval_policy: this.permission.approval_policy ?? "on-request",
      sandbox_mode: danger ? "danger-full-access" : "workspace-write",
      permission_profile: danger ? "danger_full_access" : "workspace_write",
      filesystem_scope: danger ? "host" : "workspace",
      network: this.permission.network === "allow" ? "allowed" : "blocked",
      tool_policy_id: danger ? "allow-all" : "workspace",
      mcp_servers: [],
      memory_scope: "profile-session",
    };
  }

  async close() {}
}

function transportFor(options, recorder) {
  if (options.transport === "ws") return new WsTransport(options, recorder);
  if (options.transport === "stdio") return new StdioTransport(options, recorder);
  return new FixtureTransport(options, recorder);
}

function methodSetFromCapabilities(result) {
  const methods = result?.capabilities?.supported_methods ?? result?.supported_methods ?? [];
  return new Set(Array.isArray(methods) ? methods : []);
}

function isUnsupported(error) {
  if (!(error instanceof RpcError)) return false;
  return error.code === -32004 || error.code === -32601;
}

function caseRecord(name, status, extra = {}) {
  return { name, status, ...extra };
}

async function callRpc({ transport, recorder, method, params, caseName }) {
  const id = `m12-${Date.now()}-${randomUUID().slice(0, 8)}`;
  recorder.recordTx(id, method, params, caseName);
  try {
    const result = await transport.request(id, method, params);
    recorder.recordRx(id, method, { ok: true, result: resultSummary(method, result) }, caseName);
    return { ok: true, result };
  } catch (error) {
    const rpcError = rpcErrorObject(error);
    recorder.recordRx(id, method, { ok: false, error: rpcError }, caseName);
    return { ok: false, error, rpcError };
  }
}

function mergePolicyStamp(base, overlay) {
  if (!base || typeof base !== "object") return overlay;
  return { ...base, ...overlay };
}

async function main() {
  const options = parseArgs(process.argv);
  ensureDir(options.outDir);
  ensureDir(options.workspace);
  ensureDir(options.dataDir);
  const recorder = new Recorder(options);
  recorder.resetArtifacts();

  const summary = {
    schema: "octos-m12-solo-appui-soak-v1",
    started_at: nowIso(),
    transport: options.transport,
    endpoint: options.transport === "ws" ? options.endpoint : undefined,
    workspace: path.resolve(options.workspace),
    data_dir: path.resolve(options.dataDir),
    profile_id: options.profileId,
    session_id: options.sessionId,
    local_onboarding: {
      method: "profile/local/create",
      username: options.localUsername,
      email_metadata_only: options.localEmail,
    },
    cases: [],
    blockers: [],
    failures: [],
    artifacts: {
      "appui-transcript": "appui-transcript.jsonl",
      "runtime-policy-stamp": "runtime-policy-stamp.json",
      "tool-registry-snapshot": "tool-registry-snapshot.json",
      "approval-events": "approval-events.jsonl",
      "filesystem-probe": "filesystem-probe.json",
    },
  };

  const transport = transportFor(options, recorder);
  await transport.connect();
  let latestPolicyStamp = {};
  let latestToolRegistry = {};
  let capabilities = new Set();

  const cap = await callRpc({
    transport,
    recorder,
    method: "config/capabilities/list",
    params: {},
    caseName: "capability-check",
  });
  if (cap.ok) {
    capabilities = methodSetFromCapabilities(cap.result);
    summary.cases.push(caseRecord("capability-check", "ok", {
      supports_profile_local_create: capabilities.has("profile/local/create"),
      supports_permission_profile_list: capabilities.has("permission/profile/list"),
      supports_permission_profile_set: capabilities.has("permission/profile/set"),
    }));
  } else {
    summary.blockers.push({
      area: "M12-B",
      reason: "config/capabilities/list failed",
      error: cap.rpcError,
    });
    summary.cases.push(caseRecord("capability-check", "blocked", { error: cap.rpcError }));
  }

  const localCreate = await callRpc({
    transport,
    recorder,
    method: "profile/local/create",
    params: {
      name: options.localName,
      username: options.localUsername,
      email: options.localEmail,
    },
    caseName: "local-profile-create-no-otp",
  });
  if (localCreate.ok) {
    summary.cases.push(caseRecord("local-profile-create-no-otp", "ok", {
      created: localCreate.result?.created,
      runtime_mode: localCreate.result?.runtime_mode,
      profile_id: localCreate.result?.profile_id,
    }));
  } else {
    summary.blockers.push({
      area: "M12-A",
      reason: "profile/local/create is not available or did not create the local solo profile",
      todo: "M12-A wires profile/local/create and local no-OTP owner persistence.",
      error: localCreate.rpcError,
    });
    summary.cases.push(caseRecord("local-profile-create-no-otp", "blocked", {
      error: localCreate.rpcError,
    }));
  }

  const sessionOpen = await callRpc({
    transport,
    recorder,
    method: "session/open",
    params: {
      session_id: options.sessionId,
      profile_id: options.profileId,
      cwd: path.resolve(options.workspace),
    },
    caseName: "workspace-cwd-open",
  });
  if (sessionOpen.ok) {
    summary.cases.push(caseRecord("workspace-cwd-open", "ok", {
      workspace_root: sessionOpen.result?.opened?.workspace_root,
    }));
  } else {
    summary.blockers.push({
      area: "M12-A",
      reason: "session/open with cwd failed",
      todo: "M12-A resolves local solo profile before cwd-bound session/open.",
      error: sessionOpen.rpcError,
    });
    summary.cases.push(caseRecord("workspace-cwd-open", "blocked", {
      error: sessionOpen.rpcError,
    }));
  }

  async function readStatus(caseName, overlay = {}) {
    const status = await callRpc({
      transport,
      recorder,
      method: "session/status/read",
      params: {
        session_id: options.sessionId,
        profile_id: options.profileId,
      },
      caseName,
    });
    if (status.ok) {
      latestPolicyStamp = mergePolicyStamp(status.result?.runtime_policy_stamp ?? {}, overlay);
      summary.cases.push(caseRecord(caseName, "ok", { runtime_policy_stamp: latestPolicyStamp }));
    } else {
      summary.blockers.push({
        area: "M12-B",
        reason: `${caseName}: session/status/read failed`,
        error: status.rpcError,
      });
      latestPolicyStamp = mergePolicyStamp(latestPolicyStamp, overlay);
      summary.cases.push(caseRecord(caseName, "blocked", { error: status.rpcError }));
    }
  }

  async function listOrSetPermission(caseName, method, params, m12Area, todo) {
    const response = await callRpc({ transport, recorder, method, params, caseName });
    if (response.ok) {
      summary.cases.push(caseRecord(caseName, "ok", {
        applied: response.result?.applied,
        current: response.result?.current,
      }));
      return response;
    }
    summary.blockers.push({
      area: m12Area,
      reason: `${caseName}: ${method} failed`,
      todo,
      error: response.rpcError,
    });
    summary.cases.push(caseRecord(caseName, isUnsupported(response.error) ? "blocked" : "failed", {
      error: response.rpcError,
    }));
    return response;
  }

  await readStatus("status-after-open");

  await listOrSetPermission(
    "permission-list",
    "permission/profile/list",
    { session_id: options.sessionId },
    "M12-B/M12-C",
    "M12-B exposes permission/profile/list and M12-C maps modes to runtime policy.",
  );

  await listOrSetPermission(
    "workspace-write",
    "permission/profile/set",
    { session_id: options.sessionId, update: PERMISSION_MODES.workspaceWrite },
    "M12-B/M12-C",
    "M12-B/C implement workspace-write policy selection.",
  );
  await readStatus("workspace-write-policy-stamp", {
    expected_case: "workspace-write",
  });

  await listOrSetPermission(
    "approval-never-sandbox-active",
    "permission/profile/set",
    { session_id: options.sessionId, update: PERMISSION_MODES.approvalNeverWorkspace },
    "M12-C",
    "M12-C keeps sandbox active when approval_policy=never without dangerous-full-access.",
  );
  await readStatus("approval-never-sandbox-policy-stamp", {
    expected_case: "approval-never-sandbox-active",
    expected_approval_policy: "never",
    expected_sandbox_active: true,
  });

  await listOrSetPermission(
    "danger-full-access-approval-never",
    "permission/profile/set",
    { session_id: options.sessionId, update: PERMISSION_MODES.dangerFullAccess },
    "M12-C",
    "M12-C implements danger-full-access with approval_policy=never and host filesystem scope.",
  );
  await readStatus("danger-full-access-policy-stamp", {
    expected_case: "danger-full-access-approval-never",
    expected_approval_policy: "never",
    expected_filesystem_scope: "host",
  });

  if (options.tenantNegative) {
    const tenantReject = await callRpc({
      transport,
      recorder,
      method: "permission/profile/set",
      params: {
        session_id: `${options.profileId}:tenant:m12-negative#${Date.now()}`,
        profile_id: options.profileId,
        runtime_mode: "tenant",
        update: PERMISSION_MODES.dangerFullAccess,
      },
      caseName: "tenant-danger-rejection",
    });
    if (tenantReject.ok) {
      summary.failures.push({
        area: "M12-C",
        reason: "tenant/cloud dangerous mode request was applied instead of rejected",
        result: tenantReject.result,
      });
      summary.cases.push(caseRecord("tenant-danger-rejection", "failed", {
        result: tenantReject.result,
      }));
    } else if (isUnsupported(tenantReject.error)) {
      summary.blockers.push({
        area: "M12-C",
        reason: "tenant/cloud dangerous rejection could not be observed because permission/profile/set is unavailable",
        todo: "M12-C rejects danger-full-access outside local solo mode.",
        error: tenantReject.rpcError,
      });
      summary.cases.push(caseRecord("tenant-danger-rejection", "blocked", {
        error: tenantReject.rpcError,
      }));
    } else {
      summary.cases.push(caseRecord("tenant-danger-rejection", "ok", {
        rejected: true,
        error: tenantReject.rpcError,
      }));
    }
  }

  const tools = await callRpc({
    transport,
    recorder,
    method: "tool/status/list",
    params: { session_id: options.sessionId, profile_id: options.profileId },
    caseName: "tool-registry-snapshot",
  });
  if (tools.ok) {
    latestToolRegistry = tools.result;
    summary.cases.push(caseRecord("tool-registry-snapshot", "ok", {
      policy_id: tools.result?.policy_id,
      tool_count: Array.isArray(tools.result?.tools) ? tools.result.tools.length : undefined,
    }));
  } else {
    latestToolRegistry = {
      unavailable: true,
      error: tools.rpcError,
    };
    summary.blockers.push({
      area: "M12-B",
      reason: "tool/status/list failed; cannot snapshot tool registry",
      error: tools.rpcError,
    });
    summary.cases.push(caseRecord("tool-registry-snapshot", "blocked", {
      error: tools.rpcError,
    }));
  }

  const noOtp = recorder.assertNoOtpTraffic();
  summary.no_otp_assertion = noOtp;
  if (!noOtp.ok) {
    summary.failures.push({
      area: "M12-A/M12-G",
      reason: "solo local onboarding transcript contains OTP method traffic",
      details: noOtp,
    });
  }

  const approvalPromptCount = recorder.approvalEvents.filter((event) => event.method === "approval/requested").length;
  summary.approval_events = {
    total: recorder.approvalEvents.length,
    requested: approvalPromptCount,
  };
  if (approvalPromptCount > 0) {
    summary.failures.push({
      area: "M12-C",
      reason: "approval_policy=never evidence saw approval/requested events",
      requested_count: approvalPromptCount,
    });
  }

  const filesystemProbe = {
    schema: "octos-m12-filesystem-probe-v1",
    workspace: path.resolve(options.workspace),
    cases: [
      {
        name: "workspace-write",
        expected_scope: "workspace",
        status: summary.cases.some((c) => c.name === "workspace-write" && c.status === "ok")
          ? "policy-requested"
          : "blocked",
      },
      {
        name: "approval-never-sandbox-active",
        expected_approval_policy: "never",
        expected_sandbox_active: true,
        status: summary.cases.some((c) => c.name === "approval-never-sandbox-active" && c.status === "ok")
          ? "policy-requested"
          : "blocked",
      },
      {
        name: "danger-full-access-approval-never",
        expected_scope: "host",
        expected_approval_policy: "never",
        status: summary.cases.some((c) => c.name === "danger-full-access-approval-never" && c.status === "ok")
          ? "policy-requested"
          : "blocked",
      },
    ],
    note: "Live filesystem mutation probes require M12-C shell/filesystem enforcement hooks. This runner records policy negotiation now and is ready to attach command probes when the backend exposes them.",
  };

  writeJson(recorder.paths.runtimePolicy, {
    schema: "octos-m12-runtime-policy-stamp-v1",
    captured_at: nowIso(),
    transport: options.transport,
    session_id: options.sessionId,
    profile_id: options.profileId,
    stamp: latestPolicyStamp,
    blockers: summary.blockers.filter((b) => b.area === "M12-B" || b.area === "M12-C"),
  });
  writeJson(recorder.paths.toolRegistry, {
    schema: "octos-m12-tool-registry-snapshot-v1",
    captured_at: nowIso(),
    transport: options.transport,
    session_id: options.sessionId,
    snapshot: latestToolRegistry,
  });
  writeJson(recorder.paths.filesystemProbe, filesystemProbe);

  summary.finished_at = nowIso();
  summary.status = summary.failures.length > 0
    ? "failed"
    : summary.blockers.length > 0
      ? "blocked"
      : "passed";
  writeJson(recorder.paths.summary, summary);

  await transport.close();

  const strictFailure = options.strict && summary.status !== "passed";
  if (strictFailure || summary.failures.length > 0) {
    process.stderr.write(`M12 solo AppUI probe ${summary.status}; artifacts: ${options.outDir}\n`);
    process.exit(strictFailure ? 2 : 1);
  }
  process.stdout.write(`M12 solo AppUI probe ${summary.status}; artifacts: ${options.outDir}\n`);
}

main().catch((error) => {
  process.stderr.write(`m12-solo-appui-probe failed: ${error?.stack ?? error}\n`);
  process.exit(1);
});
