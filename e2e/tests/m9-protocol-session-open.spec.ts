/**
 * M9 wire-level e2e: `session/open`.
 *
 * Issue: https://github.com/octos-org/octos/issues/647
 * Spec  : api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md §7 Command Semantics
 *
 * Asserts envelope shape, error codes and cursor monotonicity ONLY — no
 * rendered DOM. Each test mints its own session id and tears down its own
 * socket so it is independently runnable:
 *
 *   OCTOS_LIVE_TOKEN=… npx playwright test tests/m9-protocol-session-open.spec.ts
 *
 * The fault-injection variants of the cursor checks are duplicated by
 * `m9-protocol-fault-injection.spec.ts` so each spec file remains a useful
 * gate on its own — duplication is cheap, missing coverage is not.
 */
import { test, expect } from "@playwright/test";
import {
  M9WsClient,
  RPC_ERROR_CODES,
  expectRpcError,
  liveServerEnv,
  uniqueSessionId,
} from "../lib/m9-ws-client";

test.describe("M9 protocol — session/open", () => {
  test.setTimeout(60_000);

  test("opens a fresh session and returns an opened envelope with a baseline cursor", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient(env);
    const sid = uniqueSessionId("m9-open");
    try {
      const result = await client.openSession({ session_id: sid });

      // Envelope shape: SessionOpenResult { opened: SessionOpened }
      expect(result).toBeTruthy();
      expect(result.opened).toBeTruthy();
      expect(result.opened.session_id).toBe(sid);

      // Baseline cursor: stream === session id, seq advances from 0 to ≥1.
      expect(result.opened.cursor).toBeTruthy();
      expect(result.opened.cursor!.stream).toBe(sid);
      expect(typeof result.opened.cursor!.seq).toBe("number");
      expect(result.opened.cursor!.seq).toBeGreaterThanOrEqual(1);

      // The server publishes a `session/open` notification mirroring the
      // SessionOpened payload right after the response — wait for it so
      // we can prove cursor monotonicity below.
      const opened = await client.waitForNotification("session/open", 10_000);
      expect(opened.params.session_id).toBe(sid);
      expect(opened.params.cursor?.stream).toBe(sid);
      expect(opened.params.cursor?.seq).toBe(result.opened.cursor!.seq);
    } finally {
      await client.close();
    }
  });

  test("resume with a valid prior cursor returns a strictly later cursor", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient(env);
    const sid = uniqueSessionId("m9-resume");
    try {
      const first = await client.openSession({ session_id: sid });
      const seq1 = first.opened.cursor!.seq;

      // Resume from the same cursor — server should accept and emit a fresh
      // SessionOpened with a strictly greater seq (cursor monotonicity).
      const second = await client.openSession({
        session_id: sid,
        after: { stream: sid, seq: seq1 },
      });
      expect(second.opened.cursor).toBeTruthy();
      expect(second.opened.cursor!.stream).toBe(sid);
      expect(second.opened.cursor!.seq).toBeGreaterThan(seq1);
    } finally {
      await client.close();
    }
  });

  test("resume with a future cursor returns typed cursor_expired", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient(env);
    const sid = uniqueSessionId("m9-future");
    try {
      // Future cursor for a brand-new session: server has retained_seq = 0.
      const err = await expectRpcError(
        () =>
          client.openSession({
            session_id: sid,
            after: { stream: sid, seq: 999_999 },
          }),
        RPC_ERROR_CODES.CURSOR_OUT_OF_RANGE,
      );
      // Server tags the data with `kind: "cursor_expired"`.
      expect(err.data?.kind).toBe("cursor_expired");
      expect(err.data?.method).toBe("session/open");
    } finally {
      await client.close();
    }
  });

  test("resume with a cursor from a different stream returns cursor_stream_mismatch", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient(env);
    const sid = uniqueSessionId("m9-stream-mismatch");
    try {
      const err = await expectRpcError(
        () =>
          client.openSession({
            session_id: sid,
            after: { stream: "different-session", seq: 0 },
          }),
        RPC_ERROR_CODES.CURSOR_INVALID,
      );
      expect(err.data?.kind).toBe("cursor_stream_mismatch");
      expect(err.data?.expected_stream).toBe(sid);
    } finally {
      await client.close();
    }
  });
});
