/**
 * M9 wire-level e2e: `session/append_message` (UPCR-2026-042).
 *
 * Spec  : docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_042_SESSION_APPEND_MESSAGE.md
 * Issue : https://github.com/octos-org/octos/issues/2355
 *
 * Asserts envelope shape, idempotent-retry semantics and live projection —
 * no rendered DOM. Each test mints its own session id and tears down its own
 * socket so it is independently runnable:
 *
 *   OCTOS_TEST_URL=… OCTOS_AUTH_TOKEN=… npx playwright test tests/m9-protocol-session-append.spec.ts
 *
 * The suite is model-independent by construction: this lane's serve has no
 * provider configured, so a record-only append SUCCEEDING here is itself the
 * proof that the write never routes through the agent loop (a turn/start
 * would fail with `profile '_main' is not configured`).
 */
import { test, expect } from "@playwright/test";
import {
  M9WsClient,
  liveServerEnv,
  uniqueSessionId,
} from "../lib/m9-ws-client";

interface AppendResult {
  session_id: string;
  seq: number;
  thread_id?: string;
}

test.describe("M9 protocol — session/append_message", () => {
  test.setTimeout(60_000);

  test("retry with the same client_message_id returns the original seq", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient({
      ...env,
      uiFeatures: ["auxiliary.rest_to_ws.v1"],
    });
    const sid = uniqueSessionId("m9-append-retry");
    try {
      await client.openSession({ session_id: sid });
      const params = {
        session_id: sid,
        role: "system",
        content: "importer checkpoint",
        source: "external_record:import",
        client_message_id: `cmid-${sid}-note`,
      };
      const first = await client.rawRequest<AppendResult>(
        "session/append_message",
        params,
      );
      expect(first.thread_id ?? null).toBeNull();
      const retry = await client.rawRequest<AppendResult>(
        "session/append_message",
        {
          ...params,
          content: "a DIFFERENT body must still lose to the first row",
        },
      );
      expect(retry.seq).toBe(first.seq);
      expect(retry.thread_id ?? null).toBeNull();

      // A role-switching retry over the UNBOUND durable row must keep
      // echoing the row's absent binding — not the retry payload's freshly
      // stamped one (a user row would root its own thread on first commit).
      const switched = await client.rawRequest<AppendResult>(
        "session/append_message",
        {
          ...params,
          role: "user",
          content: "retry body under a role that roots its own thread",
        },
      );
      expect(switched.seq).toBe(first.seq);
      expect(switched.thread_id ?? null).toBeNull();

      const page = await client.rawRequest<{
        messages: Array<{ content: string; thread_id?: string }>;
      }>("session/messages_page", { session_id: sid });
      expect(
        page.messages.filter((m) => m.content === "importer checkpoint"),
      ).toHaveLength(1);
      expect(
        page.messages.filter((m) => m.content.includes("DIFFERENT body")),
      ).toHaveLength(0);
      // The durable row never grew a thread from the retries.
      const imported = page.messages.find(
        (m) => m.content === "importer checkpoint",
      );
      expect(imported?.thread_id).toBeUndefined();
    } finally {
      await client.close();
    }
  });
});
