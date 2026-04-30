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
  /** Optional profile id sent on `session/open.profile_id`. */
  profileId?: string;
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
  > & { profileId?: string };

  constructor(opts: M9WsClientOptions) {
    const url = opts.url
      .replace(/^http:/, "ws:")
      .replace(/^https:/, "wss:")
      .replace(/\/$/, "")
      .concat(opts.url.includes("/api/ui-protocol/ws") ? "" : "/api/ui-protocol/ws");
    this.opts = {
      url,
      token: opts.token,
      connectTimeoutMs: opts.connectTimeoutMs ?? 10_000,
      requestTimeoutMs: opts.requestTimeoutMs ?? 30_000,
      profileId: opts.profileId,
    };
  }

  /** Open the socket. Resolves once the underlying transport is open. */
  async connect(): Promise<void> {
    if (this.ws) return;
    const ws = new WebSocket(this.opts.url, {
      headers: { Authorization: `Bearer ${this.opts.token}` },
    });
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
