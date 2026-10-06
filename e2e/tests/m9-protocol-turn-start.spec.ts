/**
 * M9 wire-level e2e: `turn/start` happy-path.
 *
 * Issue: https://github.com/octos-org/octos/issues/647
 * Spec  : api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md §7 / §8
 *
 * Asserts wire-level only:
 *   - `turn/start` result is `{ accepted: true }`.
 *   - The notification stream contains `turn/started`, at least one
 *     `assistant_delta` envelope, and the canonical `turn_terminal`
 *     envelope for the same `turn_id` (the raw `turn/completed` /
 *     `message/delta` frames are never delivered since #2318).
 *   - Cursors on durable notifications are strictly monotonic in seq.
 *
 * Tests are deterministic: the prompt asks for the literal token "OK" so we
 * don't depend on LLM creativity. The model identity (deepseek-v4-pro by
 * default) is whatever the live server is configured for.
 */
import { test, expect } from "@playwright/test";
import {
  M9WsClient,
  envelopePayloads,
  freshTurnId,
  isTurnTerminal,
  liveServerEnv,
  uniqueSessionId,
  waitForTurnTerminal,
} from "../lib/m9-ws-client";

test.describe("M9 protocol — turn/start", () => {
  test.setTimeout(60_000);

  test("happy path: turn/started -> assistant_delta+ -> turn_terminal envelope", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient(env);
    const sid = uniqueSessionId("m9-turn");
    const turnId = freshTurnId();
    try {
      const opened = await client.openSession({ session_id: sid });
      expect(opened.opened.session_id).toBe(sid);
      const baselineSeq = opened.opened.cursor!.seq;

      // Start the turn. Server returns { accepted: true } and then publishes
      // a series of notifications for that turn_id.
      const accept = await client.startTurn({
        session_id: sid,
        turn_id: turnId,
        // Single, deterministic instruction. The reply length is bounded so
        // total turn time stays inside the per-test budget.
        input: [
          { kind: "text", text: "Reply with the single word OK and nothing else." },
        ],
      });
      expect(accept.accepted).toBe(true);

      // Wait for the terminal envelope before asserting the rest.
      const terminal = await waitForTurnTerminal(client, turnId, 45_000);
      expect(terminal.params.turn_id).toBe(turnId);
      expect(terminal.params.session_id).toBe(sid);
      expect(terminal.params.cursor).toBeTruthy();
      expect(terminal.params.cursor.stream).toBe(sid);
      expect(terminal.params.cursor.seq).toBeGreaterThan(baselineSeq);

      // Inspect the full notification log for the turn.
      const log = client.notificationsLog();
      const forTurn = log.filter(
        (n) => n.params?.turn_id === turnId,
      );
      // turn/started must appear before the terminal envelope.
      const startedIdx = forTurn.findIndex((n) => n.method === "turn/started");
      const terminalIdx = forTurn.findIndex((n) => isTurnTerminal(n));
      expect(startedIdx).toBeGreaterThanOrEqual(0);
      expect(terminalIdx).toBeGreaterThan(startedIdx);

      // At least one streamed assistant fragment with non-empty text.
      const deltas = envelopePayloads(forTurn, "assistant_delta");
      expect(deltas.length).toBeGreaterThanOrEqual(1);
      const combined = deltas
        .map((d) => String(d.params?.payload?.data?.text ?? ""))
        .join("");
      expect(combined.length).toBeGreaterThan(0);

      // Cursor monotonicity across the durable notifications. Notifications
      // that don't carry a cursor (e.g. turn/started) are skipped.
      const cursored = forTurn
        .map((n) => n.params?.cursor?.seq)
        .filter((seq): seq is number => typeof seq === "number");
      for (let i = 1; i < cursored.length; i++) {
        expect(cursored[i]).toBeGreaterThan(cursored[i - 1]);
      }
    } finally {
      await client.close();
    }
  });
});
