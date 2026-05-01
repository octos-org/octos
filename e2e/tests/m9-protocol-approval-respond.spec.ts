/**
 * M9 wire-level e2e: `approval/respond` (error paths).
 *
 * Issue: https://github.com/octos-org/octos/issues/647
 * Spec  : api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md §7 / §8
 *
 * Happy-path approval lifecycle requires a tool-call that triggers a
 * pending approval (e.g. `shell { command: rm -rf … }`). That requires
 * sandbox/policy hooks beyond the harness scope and is therefore deferred
 * to spec authors per workstream.
 *
 * This spec covers what the harness CAN assert today:
 *
 *   - approve / deny enums round-trip in the wire payload.
 *   - approval_scope ("request" | "turn" | "session") is accepted as an
 *     additive optional field (UPCR-2026-001).
 *   - approving an unknown approval_id returns `UNKNOWN_APPROVAL_ID`.
 *   - the data envelope carries `kind`, `method`, `session_id`, `approval_id`.
 *
 * Double-respond / -32011 (`approval_not_pending`) coverage uses the default
 * M9 protocol fixture lane, which deterministically emits `approval/requested`.
 */
import { test, expect } from "@playwright/test";
import {
  M9WsClient,
  RPC_ERROR_CODES,
  expectRpcError,
  freshApprovalId,
  freshTurnId,
  liveServerEnv,
  uniqueSessionId,
} from "../lib/m9-ws-client";

function expectUnknownApprovalKind(kind: unknown): void {
  expect(kind).toBe("unknown_approval");
}

test.describe("M9 protocol — approval/respond", () => {
  test.setTimeout(60_000);

  test("approve on an unknown approval_id returns typed unknown_approval", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient(env);
    const sid = uniqueSessionId("m9-approval-unknown");
    const approvalId = freshApprovalId();
    try {
      await client.openSession({ session_id: sid });
      const err = await expectRpcError(
        () =>
          client.respondApproval({
            session_id: sid,
            approval_id: approvalId,
            decision: "approve",
          }),
        RPC_ERROR_CODES.UNKNOWN_APPROVAL_ID,
      );
      expectUnknownApprovalKind(err.data?.kind);
      expect(err.data?.method).toBe("approval/respond");
      expect(err.data?.session_id).toBe(sid);
      expect(err.data?.approval_id).toBe(approvalId);
    } finally {
      await client.close();
    }
  });

  test("deny on an unknown approval_id returns the same typed envelope", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient(env);
    const sid = uniqueSessionId("m9-approval-deny");
    try {
      await client.openSession({ session_id: sid });
      const err = await expectRpcError(
        () =>
          client.respondApproval({
            session_id: sid,
            approval_id: freshApprovalId(),
            decision: "deny",
          }),
        RPC_ERROR_CODES.UNKNOWN_APPROVAL_ID,
      );
      expectUnknownApprovalKind(err.data?.kind);
    } finally {
      await client.close();
    }
  });

  test("approval_scope and client_note are accepted as additive optional fields", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient(env);
    const sid = uniqueSessionId("m9-approval-scope");
    try {
      await client.openSession({ session_id: sid });
      // Field acceptance is observable through the error payload — bad scope
      // would surface as INVALID_PARAMS BEFORE the not-found lookup runs.
      // Each known scope value should produce UNKNOWN_APPROVAL_ID, not
      // INVALID_PARAMS, when the approval_id is unknown.
      for (const scope of ["request", "turn", "session"] as const) {
        const err = await expectRpcError(() =>
          client.respondApproval({
            session_id: sid,
            approval_id: freshApprovalId(),
            decision: "approve",
            approval_scope: scope,
            client_note: `e2e probe (${scope})`,
          }),
        );
        expect(err.code).toBe(RPC_ERROR_CODES.UNKNOWN_APPROVAL_ID);
      }
    } finally {
      await client.close();
    }
  });

  test("double-respond on a pending approval returns -32011 approval_not_pending", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient(env);
    const sid = uniqueSessionId("m9-approval-double");
    const turnId = freshTurnId();
    const prompt = "M9 approval fixture: request approval for printf m9-approval-e2e";
    try {
      await client.openSession({ session_id: sid });
      const accept = await client.startTurn({
        session_id: sid,
        turn_id: turnId,
        input: [{ kind: "text", text: prompt }],
      });
      expect(accept.accepted).toBe(true);

      const requested = await client.waitForNotification("approval/requested", 45_000);
      expect(requested.params.session_id).toBe(sid);
      expect(requested.params.turn_id).toBe(turnId);
      expect(typeof requested.params.approval_id).toBe("string");

      const first = await client.respondApproval({
        session_id: sid,
        approval_id: requested.params.approval_id,
        decision: "approve",
      });
      expect(first.accepted).toBe(true);
      expect(first.status).toBe("accepted");

      const second = await expectRpcError(
        () =>
          client.respondApproval({
            session_id: sid,
            approval_id: requested.params.approval_id,
            decision: "approve",
          }),
        RPC_ERROR_CODES.APPROVAL_NOT_PENDING,
      );
      expect(second.data?.kind).toBe("approval_not_pending");
      expect(second.data?.method).toBe("approval/respond");
      expect(second.data?.session_id).toBe(sid);
      expect(second.data?.approval_id).toBe(requested.params.approval_id);
      expect(second.data?.recorded_decision).toBe("approve");

      await Promise.race([
        client.waitForNotification("turn/completed", 45_000),
        client.waitForNotification("turn/error", 45_000),
      ]).catch(() => {
        // The idempotency contract is already asserted. Some approval fixtures
        // intentionally pause after approval for operator inspection.
      });
    } finally {
      await client.close();
    }
  });
});
