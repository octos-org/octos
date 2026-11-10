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
