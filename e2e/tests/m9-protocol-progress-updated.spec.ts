/**
 * M9 wire-level e2e: `progress/updated` notification stream.
 *
 * Issue: https://github.com/octos-org/octos/issues/647
 * Spec  : api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md §8
 *
 * `progress/updated` carries typed lifecycle metadata (thinking, response,
 * stream_end, token_cost_update, etc.). The tests stay at the wire level:
 * every progress payload must decode as `{ metadata: { kind: <registry> } }`
 * and any typed nested metadata must have the expected JSON shape.
 */
import { test, expect } from "@playwright/test";
import {
  M9WsClient,
  freshTurnId,
  liveServerEnv,
  uniqueSessionId,
} from "../lib/m9-ws-client";

const PROGRESS_METADATA_KINDS = new Set([
  "status",
  "thinking",
  "response",
  "stream_end",
  "retry_backoff",
  "file_mutation",
  "token_cost_update",
  "tool_progress",
  "tool_completed",
  "unknown",
]);

test.describe("M9 protocol — progress/updated", () => {
  test.setTimeout(60_000);

  test("a happy-path turn emits typed progress/updated metadata for the same turn_id", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient(env);
    const sid = uniqueSessionId("m9-progress");
    const turnId = freshTurnId();
    try {
      await client.openSession({ session_id: sid });
      await client.startTurn({
        session_id: sid,
        turn_id: turnId,
        input: [{ kind: "text", text: "Reply with the single word OK." }],
      });
      await client.waitForNotification("turn/completed", 45_000);

      const log = client.notificationsLog();
      const progress = log.filter(
        (n) => n.method === "progress/updated" && n.params?.turn_id === turnId,
      );
      expect(progress.length).toBeGreaterThanOrEqual(1);

      // Every progress payload carries a session_id and a turn_id.
      for (const p of progress) {
        expect(p.params.session_id).toBe(sid);
        expect(p.params.turn_id).toBe(turnId);

        const metadata = p.params.metadata;
        expect(metadata).toBeTruthy();
        expect(typeof metadata.kind).toBe("string");
        expect(
          PROGRESS_METADATA_KINDS.has(metadata.kind),
          `unexpected progress metadata kind: ${metadata.kind}`,
        ).toBe(true);

        if (metadata.retry !== undefined) {
          expect(typeof metadata.retry).toBe("object");
          if (metadata.retry.attempt !== undefined) {
            expect(typeof metadata.retry.attempt).toBe("number");
          }
          if (metadata.retry.max_attempts !== undefined) {
            expect(typeof metadata.retry.max_attempts).toBe("number");
          }
          if (metadata.retry.backoff_ms !== undefined) {
            expect(typeof metadata.retry.backoff_ms).toBe("number");
          }
        }

        if (metadata.token_cost !== undefined) {
          expect(typeof metadata.token_cost).toBe("object");
          for (const key of ["input_tokens", "output_tokens"] as const) {
            if (metadata.token_cost[key] !== undefined) {
              expect(typeof metadata.token_cost[key]).toBe("number");
            }
          }
          if (metadata.token_cost.currency !== undefined) {
            expect(typeof metadata.token_cost.currency).toBe("string");
          }
        }

        if (metadata.file_mutation !== undefined) {
          expect(typeof metadata.file_mutation).toBe("object");
          expect(typeof metadata.file_mutation.path).toBe("string");
        }
      }
    } finally {
      await client.close();
    }
  });
});
