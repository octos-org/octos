/**
 * M9 wire-level e2e: `turn/interrupt` happy-path.
 *
 * Issue: https://github.com/octos-org/octos/issues/647
 * Spec  : api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md §7
 *
 * This file covers interrupt idempotency and deterministic terminal drain.
 *
 *   - `turn/interrupt` against an unknown turn_id returns a typed
 *     `unknown_turn` RPC error.
 *   - `turn/interrupt` against a finished turn returns
 *     `{ interrupted: false }` again (also idempotent).
 *   - double interrupt against an in-flight turn returns idempotent success
 *     and emits exactly one terminal event.
 */
import { test, expect } from "@playwright/test";
import {
  M9WsClient,
  expectRpcError,
  freshTurnId,
  liveServerEnv,
  uniqueSessionId,
} from "../lib/m9-ws-client";

const UNKNOWN_TURN_CODE = -32101;

async function waitForTurnTerminal(
  client: M9WsClient,
  turnId: string,
  timeoutMs = 45_000,
) {
  const existing = client
    .notificationsLog()
    .find(
      (n) =>
        (n.method === "turn/completed" || n.method === "turn/error") &&
        n.params?.turn_id === turnId,
    );
  if (existing) return existing;

  return new Promise<ReturnType<M9WsClient["notificationsLog"]>[number]>((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error(`timed out waiting for terminal event for ${turnId}`)),
      timeoutMs,
    );
    client.onNotification((n) => {
      if (
        (n.method === "turn/completed" || n.method === "turn/error") &&
        n.params?.turn_id === turnId
      ) {
        clearTimeout(timer);
        resolve(n);
      }
    });
  });
}

test.describe("M9 protocol — turn/interrupt (happy paths)", () => {
  test.setTimeout(60_000);

  test("interrupting an unknown turn_id returns typed unknown_turn", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient(env);
    const sid = uniqueSessionId("m9-int-unknown");
    const turnId = freshTurnId();
    try {
      await client.openSession({ session_id: sid });
      const err = await expectRpcError(
        () =>
          client.interruptTurn({
            session_id: sid,
            turn_id: turnId,
          }),
        UNKNOWN_TURN_CODE,
      );
      expect(err.data?.kind).toBe("unknown_turn");
      expect(err.data?.turn_id).toBe(turnId);
    } finally {
      await client.close();
    }
  });

  test("interrupting a turn that already completed is idempotent (interrupted=false)", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient(env);
    const sid = uniqueSessionId("m9-int-completed");
    const turnId = freshTurnId();
    try {
      await client.openSession({ session_id: sid });
      const accept = await client.startTurn({
        session_id: sid,
        turn_id: turnId,
        input: [{ kind: "text", text: "Reply with the single word OK." }],
      });
      expect(accept.accepted).toBe(true);

      // Wait for terminal state, THEN interrupt — server should still ack
      // without an error and without producing a second turn/completed.
      await waitForTurnTerminal(client, turnId, 45_000);

      const r = await client.interruptTurn({
        session_id: sid,
        turn_id: turnId,
      });
      expect(r.interrupted).toBe(false);

      // Calling again must remain idempotent.
      const r2 = await client.interruptTurn({
        session_id: sid,
        turn_id: turnId,
      });
      expect(r2.interrupted).toBe(false);
    } finally {
      await client.close();
    }
  });

  test("double interrupt against an in-flight turn is idempotent and emits one terminal event", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient(env);
    const sid = uniqueSessionId("m9-int-double");
    const turnId = freshTurnId();
    try {
      await client.openSession({ session_id: sid });
      const accept = await client.startTurn({
        session_id: sid,
        turn_id: turnId,
        input: [
          {
            kind: "text",
            text:
              "Do not use tools. Write the word OK on 200 separate lines, one line at a time.",
          },
        ],
      });
      expect(accept.accepted).toBe(true);

      const [first, second] = await Promise.all([
        client.interruptTurn({ session_id: sid, turn_id: turnId }, 10_000),
        client.interruptTurn({ session_id: sid, turn_id: turnId }, 10_000),
      ]);
      expect(first.interrupted).toBe(true);
      expect(second.interrupted).toBe(true);

      const terminal = await waitForTurnTerminal(client, turnId, 45_000);
      expect(terminal.method).toBe("turn/error");
      expect(terminal.params.code).toBe("interrupted");

      const terminalEvents = client.notificationsLog().filter(
        (n) =>
          (n.method === "turn/completed" || n.method === "turn/error") &&
          n.params?.turn_id === turnId,
      );
      expect(terminalEvents).toHaveLength(1);
    } finally {
      await client.close();
    }
  });
});
