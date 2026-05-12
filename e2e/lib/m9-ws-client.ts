/**
 * M9 wire-level test client for the Octos UI Protocol v1
 * (`octos-ui/v1alpha1`) — JSON-RPC 2.0 over WebSocket served from
 * `/api/ui-protocol/ws`.
 *
 * Thin typed wrapper. Types mirror the serde shapes in
 * `crates/octos-core/src/ui_protocol.rs` by hand. Every spec under
 * `e2e/tests/m9-protocol-*.spec.ts` constructs one client, exercises the
 * methods it cares about, then `close()`s. Specs assert wire-level only:
 * envelope shape, error codes, cursor monotonicity — not rendered DOM.
 */
import WebSocket from "ws";
import { randomBytes, randomUUID } from "node:crypto";

// ---- wire types -----------------------------------------------------------

export interface UiCursor { stream: string; seq: number }
export type TurnId = string;
export type ApprovalId = string;
export type PreviewId = string;
export type TaskId = string;
export type InputItem = { kind: "text"; text: string };
export type ApprovalDecision = "approve" | "deny";

export interface SessionOpenParams {
  session_id: string;
  profile_id?: string;
  after?: UiCursor;
  cwd?: string;
}
export interface TurnStartParams {
  session_id: string;
  turn_id: TurnId;
  input: InputItem[];
}
export interface TurnInterruptParams { session_id: string; turn_id: TurnId }
export interface ApprovalRespondParams {
  session_id: string;
  approval_id: ApprovalId;
  decision: ApprovalDecision;
  approval_scope?: "request" | "turn" | "session";
  client_note?: string;
}
export interface DiffPreviewGetParams { session_id: string; preview_id: PreviewId }
export interface TaskOutputReadParams {
  session_id: string;
  task_id: TaskId;
  cursor?: { offset: number };
  limit_bytes?: number;
}

export interface SessionOpened {
  session_id: string;
  active_profile_id?: string;
  cursor?: UiCursor;
  panes?: unknown;
  workspace_root?: string;
}
export interface SessionOpenResult { opened: SessionOpened }
export interface TurnStartResult { accepted: boolean }
export interface TurnInterruptResult { interrupted: boolean }
export interface ApprovalRespondResult {
  approval_id: ApprovalId;
  accepted: boolean;
  status: "accepted";
  runtime_resumed?: boolean;
}
export interface DiffPreviewFile {
  path: string;
  old_path?: string;
  status: "added" | "modified" | "deleted" | "renamed";
  hunks?: Array<{ header: string; lines?: unknown[] }>;
}
export interface DiffPreview {
  session_id: string;
  preview_id: PreviewId;
  title?: string;
  files?: DiffPreviewFile[];
}
export interface DiffPreviewGetResult {
  status: "ready";
  source: "pending_store";
  preview: DiffPreview;
}
export interface TaskOutputReadResult {
  session_id: string;
  task_id: TaskId;
  source: "runtime_projection";
  cursor: { offset: number };
  next_cursor: { offset: number };
  text: string;
  bytes_read: number;
  total_bytes: number;
  truncated: boolean;
  complete: boolean;
  live_tail_supported: boolean;
  // Audit issue #707 / accepted UPCR-2026-006: true when the result was
  // projected from the task ledger snapshot rather than a live disk-routed
  // output stream. Cursors returned alongside `is_snapshot_projection: true`
  // are advisory; a fresh read may produce a different snapshot.
  is_snapshot_projection: boolean;
  task_status: string;
  runtime_state: string;
  lifecycle_state: string;
  output_files?: string[];
  limitations: Array<{ code: string; message: string }>;
}

export interface RpcError { code: number; message: string; data?: any }
export interface UiNotification { jsonrpc: "2.0"; method: string; params: any }

/** JSON-RPC error codes used by the M9 runtime slice. */
export const RPC_ERROR_CODES = {
  PARSE_ERROR: -32700,
  INVALID_REQUEST: -32600,
  METHOD_NOT_FOUND: -32601,
  INVALID_PARAMS: -32602,
  INTERNAL_ERROR: -32603,
  METHOD_NOT_SUPPORTED: -32004,
  APPROVAL_NOT_PENDING: -32011,
  UNKNOWN_SESSION: -32100,
  UNKNOWN_TURN: -32101,
  UNKNOWN_APPROVAL_ID: -32102,
  UNKNOWN_PREVIEW_ID: -32103,
  UNKNOWN_TASK_ID: -32104,
  APPROVAL_CANCELLED: -32105,
  CURSOR_OUT_OF_RANGE: -32110,
  CURSOR_INVALID: -32111,
  PERMISSION_DENIED: -32120,
  UNSUPPORTED_CAPABILITY: -32130,
  RUNTIME_NOT_READY: -32140,
  MALFORMED_RESULT: -32150,
  RATE_LIMITED: -32160,
  // CLI transport-local; no core typed code exists yet.
  FRAME_TOO_LARGE: -32005,
} as const;

// ---- client ---------------------------------------------------------------

export interface M9WsClientOptions {
  /** http://, https://, ws://, or wss:// URL — scheme is normalized. */
  url: string;
  /** Bearer token. */
  token: string;
  /** Optional profile id sent on `session/open.profile_id`. Also forwarded
   *  as the `X-Profile-Id` handshake header so server-side handlers that
   *  resolve profile scope from headers (REST-equivalent aux methods like
   *  `session/list`) honour the caller's profile, mirroring the retired
   *  REST endpoints. */
  profileId?: string;
  /** Capability features to negotiate via `ui_feature=` query params on the
   *  WS handshake (UPCR-2026-007 / M12 Phase D-1). The server rejects
   *  strictly-gated methods like `session/list`, `session/messages_page`,
   *  `session/tasks.list`, and `session/delete` with `method_not_supported`
   *  unless the matching `auxiliary.rest_to_ws.v1` feature is negotiated.
   *  Defaults to `[]` (no negotiation), keeping legacy callers unchanged. */
  uiFeatures?: readonly string[];
  connectTimeoutMs?: number;
  requestTimeoutMs?: number;
}

type Pending = {
  resolve: (value: any) => void;
  reject: (err: Error) => void;
  timer: NodeJS.Timeout;
};

export class M9WsClient {
  private ws: WebSocket | null = null;
  private pending = new Map<string, Pending>();
  private notificationHandlers: Array<(n: UiNotification) => void> = [];
  private notifications: UiNotification[] = [];
  private cursor: UiCursor | undefined;
  private closed = false;
  private readonly opts: Required<
    Pick<M9WsClientOptions, "url" | "token" | "connectTimeoutMs" | "requestTimeoutMs">
  > & { profileId?: string; uiFeatures: readonly string[] };

  constructor(opts: M9WsClientOptions) {
    // Normalize scheme and path, then append `ui_feature=<feat>` query
    // params per UPCR-2026-007. The server accepts both repeated query
    // entries and an `X-Octos-Ui-Features` header; we use the query form
    // because some WebSocket clients (and proxies) drop custom request
    // headers on the upgrade. We forward via the header anyway as a
    // belt-and-braces fallback below.
    const features = (opts.uiFeatures ?? []).filter((f) => f && f.trim() !== "");
    let url = opts.url
      .replace(/^http:/, "ws:")
      .replace(/^https:/, "wss:")
      .replace(/\/$/, "")
      .concat(opts.url.includes("/api/ui-protocol/ws") ? "" : "/api/ui-protocol/ws");
    if (features.length > 0) {
      const separator = url.includes("?") ? "&" : "?";
      const qs = features
        .map((f) => `ui_feature=${encodeURIComponent(f)}`)
        .join("&");
      url = `${url}${separator}${qs}`;
    }
    this.opts = {
      url,
      token: opts.token,
      connectTimeoutMs: opts.connectTimeoutMs ?? 10_000,
      requestTimeoutMs: opts.requestTimeoutMs ?? 30_000,
      profileId: opts.profileId,
      uiFeatures: features,
    };
  }

  /** Open the socket. Resolves once the underlying transport is open. */
  async connect(): Promise<void> {
    if (this.ws) return;
    const headers: Record<string, string> = {
      Authorization: `Bearer ${this.opts.token}`,
    };
    // Belt-and-braces: also send the negotiation as a request header so
    // server-side gates accept the handshake even when an intermediary
    // strips query-string features (the server checks header OR query).
    if (this.opts.uiFeatures.length > 0) {
      headers["X-Octos-Ui-Features"] = this.opts.uiFeatures.join(",");
    }
    // Forward profile scope on the handshake so handlers that resolve
    // routing via `routed_profile_id_from_headers` (`session/list`,
    // `session/delete`, etc.) match the retired REST endpoints, which
    // also keyed off `X-Profile-Id`.
    if (this.opts.profileId) {
      headers["X-Profile-Id"] = this.opts.profileId;
    }
    const ws = new WebSocket(this.opts.url, { headers });
    this.ws = ws;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        ws.close();
        reject(new Error(`m9-ws: connect timeout after ${this.opts.connectTimeoutMs}ms`));
      }, this.opts.connectTimeoutMs);
      ws.once("open", () => { clearTimeout(timer); resolve(); });
      ws.once("error", (err: Error) => {
        clearTimeout(timer);
        reject(new Error(`m9-ws: connect error: ${err.message}`));
      });
      ws.on("message", (data: WebSocket.RawData) => this.handleMessage(data.toString()));
      ws.on("close", () => {
        this.closed = true;
        for (const [, p] of this.pending) {
          clearTimeout(p.timer);
          p.reject(new Error("m9-ws: socket closed before response"));
        }
        this.pending.clear();
      });
    });
  }

  private handleMessage(text: string): void {
    let parsed: any;
    try { parsed = JSON.parse(text); } catch { return; }

    if (parsed && typeof parsed === "object" && "id" in parsed && parsed.id != null) {
      // Response (success or error) for one of our requests.
      const id = String(parsed.id);
      const p = this.pending.get(id);
      if (!p) return;
      this.pending.delete(id);
      clearTimeout(p.timer);
      if (parsed.error) {
        const err: RpcError = parsed.error;
        p.reject(new RpcErrorImpl(err.code, err.message, err.data));
      } else {
        p.resolve(parsed.result);
      }
      return;
    }

    if (parsed && typeof parsed === "object" && parsed.method) {
      const n: UiNotification = parsed;
      this.notifications.push(n);
      const c = n.params?.cursor;
      if (c && typeof c.seq === "number" && typeof c.stream === "string") {
        this.cursor = { stream: c.stream, seq: c.seq };
      }
      for (const h of this.notificationHandlers) {
        try { h(n); } catch { /* swallow */ }
      }
    }
  }

  /** Subscribe to every notification. */
  onNotification(handler: (n: UiNotification) => void): void {
    this.notificationHandlers.push(handler);
  }

  /** Snapshot of every notification observed so far, in receive order. */
  notificationsLog(): readonly UiNotification[] { return this.notifications; }

  /** Latest known cursor (from a notification or `session/open` result). */
  getCurrentCursor(): UiCursor | undefined { return this.cursor; }

  /**
   * Resolve when a notification of `method` arrives. If one was already seen,
   * the most-recent matching entry is returned synchronously.
   */
  async waitForNotification(method: string, timeoutMs = 30_000): Promise<UiNotification> {
    const existing = this.notifications.find((n) => n.method === method);
    if (existing) return existing;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.notificationHandlers = this.notificationHandlers.filter((h) => h !== handler);
        reject(new Error(`m9-ws: timeout waiting for notification ${method}`));
      }, timeoutMs);
      const handler = (n: UiNotification) => {
        if (n.method !== method) return;
        clearTimeout(timer);
        this.notificationHandlers = this.notificationHandlers.filter((h) => h !== handler);
        resolve(n);
      };
      this.notificationHandlers.push(handler);
    });
  }

  private async request<T>(method: string, params: any, timeoutMs?: number): Promise<T> {
    if (!this.ws) await this.connect();
    if (!this.ws || this.closed) throw new Error("m9-ws: socket not open");
    const id = `req-${Date.now()}-${randomBytes(2).toString("hex")}`;
    const frame = JSON.stringify({ jsonrpc: "2.0", id, method, params });
    const tmo = timeoutMs ?? this.opts.requestTimeoutMs;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`m9-ws: timeout waiting for ${method} after ${tmo}ms`));
      }, tmo);
      this.pending.set(id, { resolve, reject, timer });
      this.ws!.send(frame, (err) => {
        if (err) {
          this.pending.delete(id);
          clearTimeout(timer);
          reject(new Error(`m9-ws: send failed: ${err.message}`));
        }
      });
    });
  }

  /**
   * Send a fully-formed JSON-RPC frame as-is. For fault-injection specs that
   * need to test malformed envelopes (unknown method, malformed UUIDs, etc.).
   */
  async rawRequest<T>(method: string, params: any, timeoutMs?: number): Promise<T> {
    return this.request<T>(method, params, timeoutMs);
  }

  async openSession(p: SessionOpenParams, timeoutMs?: number): Promise<SessionOpenResult> {
    const params: SessionOpenParams = { ...p, profile_id: p.profile_id ?? this.opts.profileId };
    const r = await this.request<SessionOpenResult>("session/open", params, timeoutMs);
    if (r?.opened?.cursor) this.cursor = r.opened.cursor;
    return r;
  }
  async startTurn(p: TurnStartParams, timeoutMs?: number): Promise<TurnStartResult> {
    return this.request<TurnStartResult>("turn/start", p, timeoutMs);
  }
  async interruptTurn(p: TurnInterruptParams, timeoutMs?: number): Promise<TurnInterruptResult> {
    return this.request<TurnInterruptResult>("turn/interrupt", p, timeoutMs);
  }
  async respondApproval(p: ApprovalRespondParams, timeoutMs?: number): Promise<ApprovalRespondResult> {
    return this.request<ApprovalRespondResult>("approval/respond", p, timeoutMs);
  }
  async getDiffPreview(p: DiffPreviewGetParams, timeoutMs?: number): Promise<DiffPreviewGetResult> {
    return this.request<DiffPreviewGetResult>("diff/preview/get", p, timeoutMs);
  }
  async readTaskOutput(p: TaskOutputReadParams, timeoutMs?: number): Promise<TaskOutputReadResult> {
    return this.request<TaskOutputReadResult>("task/output/read", p, timeoutMs);
  }

  /** Close the underlying socket. Idempotent. */
  async close(): Promise<void> {
    if (!this.ws) return;
    if (this.closed) { this.ws = null; return; }
    return new Promise((resolve) => {
      const ws = this.ws!;
      this.ws = null;
      this.closed = true;
      for (const [, p] of this.pending) {
        clearTimeout(p.timer);
        p.reject(new Error("m9-ws: client closed"));
      }
      this.pending.clear();
      ws.once("close", () => resolve());
      try { ws.close(); } catch { resolve(); }
      // Belt and braces: resolve even if the close event never fires.
      setTimeout(() => resolve(), 1000);
    });
  }
}

// ---- helpers --------------------------------------------------------------

export function uniqueSessionId(prefix = "smoke"): string {
  return `${prefix}-${Date.now()}-${randomBytes(4).toString("hex")}`;
}
export function freshTurnId(): TurnId { return randomUUID(); }
export function freshApprovalId(): ApprovalId { return randomUUID(); }
export function freshPreviewId(): PreviewId { return randomUUID(); }
export function freshTaskId(): TaskId { return randomUUID(); }

/** Read live-server URL + token from the standard env vars. */
export function liveServerEnv(): { url: string; token: string; profileId?: string } {
  const url = process.env.OCTOS_LIVE_URL || "http://127.0.0.1:56831";
  const token =
    process.env.OCTOS_LIVE_TOKEN ||
    process.env.OCTOS_AUTH_TOKEN ||
    process.env.OCTOS_TEST_TOKEN ||
    "";
  if (!token) {
    throw new Error(
      "m9-ws: OCTOS_LIVE_TOKEN (or OCTOS_AUTH_TOKEN) must be set to run the protocol harness.",
    );
  }
  return { url, token, profileId: process.env.OCTOS_LIVE_PROFILE };
}

/** True if a typed RPC error has the expected JSON-RPC code. */
export function isRpcError(err: unknown, code?: number): err is RpcErrorImpl {
  return err instanceof RpcErrorImpl && (code === undefined || err.code === code);
}

/**
 * "Expect this call to throw a typed RPC error". Returns the captured
 * RpcErrorImpl so the spec can drill into `code`/`data`. The default
 * Playwright `expect(...).rejects` flow doesn't have `.toSatisfy()`, and
 * `.toThrow()` only matches by message — we want structural assertions.
 */
export async function expectRpcError(
  call: () => Promise<unknown>,
  code?: number,
): Promise<RpcErrorImpl> {
  let captured: unknown;
  try { await call(); } catch (err) { captured = err; }
  if (captured === undefined) throw new Error("expectRpcError: call did not throw");
  if (!(captured instanceof RpcErrorImpl)) {
    throw new Error(
      `expectRpcError: call threw non-RPC error: ${(captured as Error)?.message ?? captured}`,
    );
  }
  if (code !== undefined && captured.code !== code) {
    throw new Error(
      `expectRpcError: expected code ${code}, got ${captured.code} (${captured.message})`,
    );
  }
  return captured;
}

/** Internal: an Error that carries an RPC code/data payload. */
export class RpcErrorImpl extends Error {
  readonly code: number;
  readonly data: any;
  constructor(code: number, message: string, data?: any) {
    super(`rpc-error[${code}] ${message}`);
    this.code = code;
    this.data = data;
    this.name = "RpcErrorImpl";
  }
}

// ---------------------------------------------------------------------------
// chatWS — drop-in replacement for the legacy chatSSE() helpers used across
// the e2e suite. Issues #836 (M9-α-7), #845 (audit lock).
//
// Design contract:
//
// 1. Same return shape as chatSSE: `{ events, content, doneEvent }` so spec
//    bodies can swap `chatSSE(...)` -> `chatWS(...)` without rewriting their
//    assertions, except for SSE-only fields explicitly flagged in the
//    α-3 deferred-set follow-up (token usage, has_bg_tasks, session_result,
//    file events). Specs that need those still use test.fixme until the
//    server publishes the corresponding WS notifications (γ-3 / β-1).
//
// 2. We do NOT keep an SSE socket open in parallel — that would defeat
//    α-5/α-6 (atomic SSE delete). Every byte we observe is a JSON-RPC
//    notification on the M9 WebSocket.
//
// 3. WS notification -> synthesized SSE-shaped event mapping:
//
//      message/delta              -> { type: 'token', text }
//      tool/started               -> { type: 'tool_start', tool, tool_call_id, arguments }
//      tool/progress              -> { type: 'tool_progress', tool, tool_call_id, message, progress_pct }
//      tool/completed             -> { type: 'tool_completed', tool, tool_call_id, ... }
//      task/updated               -> { type: 'task_updated', task_id, status, ... }
//      task/output/delta          -> { type: 'task_output_delta', task_id, text }
//      progress/updated (α-4)     -> { type: 'progress_updated', ... }
//      warning                    -> { type: 'warning', message }
//      turn/started               -> { type: 'turn_started', turn_id }
//      turn/completed             -> { type: 'done', content, cursor }
//      turn/error                 -> { type: 'error', message }
//
//    Cumulative `content` is the concatenation of every `message/delta.text`
//    for the active turn, matching what SSE callers see in `done.content`.
// ---------------------------------------------------------------------------

import type { Server as HttpServer } from "node:http"; // eslint-disable-line @typescript-eslint/no-unused-vars

export interface ChatWsEvent {
  type: string;
  [key: string]: unknown;
}

export interface ChatWsResult {
  events: ChatWsEvent[];
  content: string;
  doneEvent?: ChatWsEvent;
}

export interface ChatWsOptions {
  /** http(s):// or ws(s):// base URL. Path is appended automatically. */
  baseUrl: string;
  token: string;
  message: string;
  sessionId: string;
  /** Optional profile id forwarded as `session/open.profile_id`. */
  profileId?: string;
  /** Total wait budget in ms before forcing return without `turn/completed`. */
  maxWait?: number;
  /** Override the WS connect timeout (default 10s). */
  connectTimeoutMs?: number;
  /** Override the request timeout for individual JSON-RPC calls. */
  requestTimeoutMs?: number;
  /** Reuse an existing client. If provided, caller owns close(). */
  client?: M9WsClient;
  /** Override the freshly generated turn id (defaults to a random uuid). */
  turnId?: string;
}

/**
 * Drive a single chat turn over the M9 WebSocket UI Protocol and collect
 * synthesized SSE-shaped events until `turn/completed` (or `turn/error`,
 * or the `maxWait` budget is exhausted).
 *
 * The promise resolves on the first terminal event for the turn, OR when the
 * deadline elapses, whichever comes first.
 */
export async function chatWS(opts: ChatWsOptions): Promise<ChatWsResult> {
  const ownsClient = !opts.client;
  const client =
    opts.client ??
    new M9WsClient({
      url: opts.baseUrl,
      token: opts.token,
      profileId: opts.profileId,
      connectTimeoutMs: opts.connectTimeoutMs,
      requestTimeoutMs: opts.requestTimeoutMs,
    });
  const turnId = opts.turnId ?? freshTurnId();
  const deadline = Date.now() + (opts.maxWait ?? 60_000);
  const events: ChatWsEvent[] = [];
  let content = "";
  let doneEvent: ChatWsEvent | undefined;
  let terminal = false;
  let resolveTerminal!: () => void;
  const terminalPromise = new Promise<void>((r) => {
    resolveTerminal = r;
  });

  const handler = (n: UiNotification) => {
    // Filter on turn_id when present; some notifications (warnings, status)
    // arrive without a turn scope and we forward them so spec bodies can see
    // them in `events` if they want.
    const params = (n.params ?? {}) as Record<string, unknown>;
    const noteTurnId = params.turn_id;
    if (noteTurnId !== undefined && noteTurnId !== turnId) return;
    switch (n.method) {
      case "turn/started": {
        events.push({ type: "turn_started", turn_id: noteTurnId, raw: params });
        break;
      }
      case "message/delta": {
        const text = String(params.text ?? "");
        if (text) {
          content += text;
          events.push({ type: "token", text });
        }
        break;
      }
      case "tool/started": {
        events.push({
          type: "tool_start",
          tool: params.tool_name,
          tool_call_id: params.tool_call_id,
          arguments: params.arguments,
        });
        break;
      }
      case "tool/progress": {
        events.push({
          type: "tool_progress",
          tool: params.tool_name,
          tool_call_id: params.tool_call_id,
          message: params.message,
          progress_pct: params.progress_pct,
        });
        break;
      }
      case "tool/completed": {
        events.push({
          type: "tool_completed",
          tool: params.tool_name,
          tool_call_id: params.tool_call_id,
          ok: params.ok,
          error: params.error,
        });
        break;
      }
      case "task/updated": {
        events.push({
          type: "task_updated",
          task_id: params.task_id,
          status: params.status,
          tool_call_id: params.tool_call_id,
          raw: params,
        });
        break;
      }
      case "task/output/delta": {
        events.push({
          type: "task_output_delta",
          task_id: params.task_id,
          text: params.text,
        });
        break;
      }
      case "progress/updated": {
        events.push({ type: "progress_updated", raw: params });
        break;
      }
      case "warning": {
        events.push({ type: "warning", message: params.message });
        break;
      }
      case "turn/completed": {
        const ev: ChatWsEvent = {
          type: "done",
          content,
          cursor: params.cursor,
          turn_id: noteTurnId,
        };
        events.push(ev);
        doneEvent = ev;
        terminal = true;
        resolveTerminal();
        break;
      }
      case "turn/error": {
        const ev: ChatWsEvent = {
          type: "error",
          message: params.message ?? params.code ?? "turn/error",
          code: params.code,
          turn_id: noteTurnId,
        };
        events.push(ev);
        doneEvent = ev;
        terminal = true;
        resolveTerminal();
        break;
      }
      default:
        break;
    }
  };

  client.onNotification(handler);

  try {
    await client.openSession({
      session_id: opts.sessionId,
      profile_id: opts.profileId,
    });
    await client.startTurn({
      session_id: opts.sessionId,
      turn_id: turnId,
      input: [{ kind: "text", text: opts.message }],
    });

    const remaining = Math.max(1_000, deadline - Date.now());
    await Promise.race([
      terminalPromise,
      new Promise<void>((r) => setTimeout(r, remaining)),
    ]);
  } catch (err) {
    // On RPC error during open/start, surface as `error` event so callers
    // see an SSE-equivalent shape rather than a thrown promise.
    const message = err instanceof Error ? err.message : String(err);
    const ev: ChatWsEvent = { type: "error", message };
    events.push(ev);
    doneEvent ??= ev;
  } finally {
    if (ownsClient) {
      try {
        await client.close();
      } catch {
        /* swallow */
      }
    }
  }

  // Best-effort post-condition: if no terminal arrived, do NOT inject a
  // synthetic done — callers historically rely on `doneEvent === undefined`
  // to detect timeouts.
  void terminal;
  return { events, content, doneEvent };
}

// ---------------------------------------------------------------------------
// Auxiliary RPC helpers (M12 Phase D-1/D-5).
//
// These wrap the WS UI Protocol replacements for the retired REST endpoints
// (`GET /api/sessions`, `GET /api/sessions/{id}/messages`, etc.) so spec
// bodies can replace their `fetch(/api/sessions...)` calls with one-shot
// async functions that own the WebSocket lifecycle.
//
// Each helper opens a fresh `M9WsClient`, issues the JSON-RPC method, and
// closes the socket. If the caller already has a connected client they can
// pass it via `opts.client` to amortize the connect cost across calls.
// ---------------------------------------------------------------------------

export interface AuxRpcOpts {
  baseUrl: string;
  token: string;
  profileId?: string;
  /** Optional pre-connected client (caller owns close()). */
  client?: M9WsClient;
  /** Override the request timeout for the JSON-RPC call. */
  requestTimeoutMs?: number;
}

/** Capability features required by every aux RPC helper in this file.
 *  The server gates the matching methods strict-opt-in: a handshake that
 *  does NOT advertise `auxiliary.rest_to_ws.v1` will receive
 *  `method_not_supported` (-32004), regardless of bearer/profile scope. */
const AUX_REST_TO_WS_FEATURES = ["auxiliary.rest_to_ws.v1"] as const;

async function withAuxClient<T>(
  opts: AuxRpcOpts,
  fn: (client: M9WsClient) => Promise<T>,
): Promise<T> {
  const ownsClient = !opts.client;
  const client =
    opts.client ??
    new M9WsClient({
      url: opts.baseUrl,
      token: opts.token,
      profileId: opts.profileId,
      requestTimeoutMs: opts.requestTimeoutMs,
      uiFeatures: AUX_REST_TO_WS_FEATURES,
    });
  try {
    return await fn(client);
  } finally {
    if (ownsClient) {
      try { await client.close(); } catch { /* swallow */ }
    }
  }
}

/** Throw a clearly-labelled error when an aux RPC returns a malformed
 *  result shape. Returning `[]` (or worse, an empty default object) would
 *  let specs pass for the wrong reason — e.g. "no sessions" when the
 *  server actually responded with an envelope we don't know how to read.
 *  Surface the failure loudly instead. */
function assertArray<T>(value: unknown, method: string, field: string): T[] {
  if (!Array.isArray(value)) {
    throw new Error(
      `m9-ws aux: ${method} returned non-array \`${field}\`: ${JSON.stringify(value)}`,
    );
  }
  return value as T[];
}

/** WS replacement for `GET /api/sessions`. Returns the same array shape.
 *  Errors (including `method_not_supported` when `auxiliary.rest_to_ws.v1`
 *  was not negotiated by the handshake) propagate to the caller — do NOT
 *  swallow them into `[]`, which would mask "capability missing" as
 *  "no sessions". */
export async function fetchSessionList(opts: AuxRpcOpts): Promise<any[]> {
  return withAuxClient(opts, async (client) => {
    const result = await client.rawRequest<{ sessions: unknown }>(
      "session/list",
      {},
    );
    return assertArray(result?.sessions, "session/list", "sessions");
  });
}

/** WS replacement for `GET /api/sessions/{id}/messages`. */
export async function fetchSessionMessages(
  opts: AuxRpcOpts & {
    sessionId: string;
    limit?: number;
    offset?: number;
    sinceSeq?: number;
    topic?: string;
  },
): Promise<any[]> {
  return withAuxClient(opts, async (client) => {
    const params: Record<string, unknown> = { session_id: opts.sessionId };
    if (opts.limit !== undefined) params.limit = opts.limit;
    if (opts.offset !== undefined) params.offset = opts.offset;
    if (opts.sinceSeq !== undefined) params.since_seq = opts.sinceSeq;
    if (opts.topic !== undefined) params.topic = opts.topic;
    const result = await client.rawRequest<{ messages: unknown }>(
      "session/messages_page",
      params,
    );
    return assertArray(result?.messages, "session/messages_page", "messages");
  });
}

/** WS replacement for `GET /api/sessions/{id}/tasks`. */
export async function fetchSessionTasks(
  opts: AuxRpcOpts & { sessionId: string; topic?: string },
): Promise<any[]> {
  return withAuxClient(opts, async (client) => {
    const params: Record<string, unknown> = { session_id: opts.sessionId };
    if (opts.topic !== undefined) params.topic = opts.topic;
    const result = await client.rawRequest<{ tasks: unknown }>(
      "session/tasks.list",
      params,
    );
    return assertArray(result?.tasks, "session/tasks.list", "tasks");
  });
}

/** WS replacement for `DELETE /api/sessions/{id}`. Resolves on success;
 *  errors (including `method_not_supported`) propagate to the caller. */
export async function deleteSessionWs(
  opts: AuxRpcOpts & { sessionId: string },
): Promise<void> {
  await withAuxClient(opts, async (client) => {
    await client.rawRequest<unknown>("session/delete", {
      session_id: opts.sessionId,
    });
  });
}
