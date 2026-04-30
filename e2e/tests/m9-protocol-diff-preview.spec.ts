/**
 * M9 wire-level e2e: `diff/preview/get`.
 *
 * Issue: https://github.com/octos-org/octos/issues/647
 * Spec  : api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md §7 / §8
 *
 * The pending-store population path requires a real tool call that emits
 * a diff approval (e.g. `apply_patch`). That depends on sandbox + diff
 * approval wiring outside the harness scope. We assert on the error path
 * the harness can observe today:
 *
 *   - get on an unknown preview_id returns -32104 (`unknown_preview`).
 *   - the error envelope carries the typed `kind`, `method`, and ids.
 *   - missing preview_id is rejected as INVALID_PARAMS at the parser level
 *     (no UUID -> serde fails).
 */
import { test, expect } from "@playwright/test";
import {
  M9WsClient,
  RPC_ERROR_CODES,
  expectRpcError,
  freshPreviewId,
  liveServerEnv,
  uniqueSessionId,
} from "../lib/m9-ws-client";

test.describe("M9 protocol — diff/preview/get", () => {
  test.setTimeout(60_000);

  test("unknown preview_id returns typed unknown_preview envelope", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient(env);
    const sid = uniqueSessionId("m9-diff-unknown");
    const previewId = freshPreviewId();
    try {
      await client.openSession({ session_id: sid });
      const err = await expectRpcError(
        () =>
          client.getDiffPreview({
            session_id: sid,
            preview_id: previewId,
          }),
        RPC_ERROR_CODES.UNKNOWN_PREVIEW_ID,
      );
      expect(err.data?.kind).toBe("unknown_preview");
      expect(err.data?.method).toBe("diff/preview/get");
      expect(err.data?.session_id).toBe(sid);
      expect(err.data?.preview_id).toBe(previewId);
    } finally {
      await client.close();
    }
  });

  test("missing preview_id is rejected as INVALID_PARAMS at the parser", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient(env);
    const sid = uniqueSessionId("m9-diff-malformed");
    try {
      await client.openSession({ session_id: sid });
      // `rawRequest` skips client-side type checks so we can probe what the
      // server's serde decoder does with a bad payload.
      const err = await expectRpcError(
        () =>
          client.rawRequest("diff/preview/get", {
            session_id: sid,
            // preview_id omitted on purpose.
          } as any),
        RPC_ERROR_CODES.INVALID_PARAMS,
      );
      // The parser includes the method name in `error.message`.
      expect(err.message.toLowerCase()).toContain("diff/preview/get");
    } finally {
      await client.close();
    }
  });

  test("malformed preview_id (not a UUID) is rejected as INVALID_PARAMS", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient(env);
    const sid = uniqueSessionId("m9-diff-bad-uuid");
    try {
      await client.openSession({ session_id: sid });
      const err = await expectRpcError(
        () =>
          client.rawRequest("diff/preview/get", {
            session_id: sid,
            preview_id: "definitely-not-a-uuid",
          }),
        RPC_ERROR_CODES.INVALID_PARAMS,
      );
      expect(err.message.toLowerCase()).toContain("diff/preview/get");
    } finally {
      await client.close();
    }
  });
});
