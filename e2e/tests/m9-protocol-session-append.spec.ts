/**
 * M9 wire-level e2e: `session/append_message` (UPCR-2026-041).
 *
 * Spec  : docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_041_SESSION_APPEND_MESSAGE.md
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
  RPC_ERROR_CODES,
  expectRpcError,
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

  test("persists user and assistant records without a turn and reads them back", async () => {
    const env = liveServerEnv();
    // The read-back leg negotiates `auxiliary.rest_to_ws.v1` — that is the
    // gate `session/messages_page` lives behind (the append method itself is
    // ungated, so the writes below work on a plain connection too).
    const client = new M9WsClient({
      ...env,
      uiFeatures: ["auxiliary.rest_to_ws.v1"],
    });
    const sid = uniqueSessionId("m9-append");
    try {
      await client.openSession({ session_id: sid });

      const user = await client.rawRequest<AppendResult>("session/append_message", {
        session_id: sid,
        role: "user",
        content: "What is the capital of Japan?",
        source: "external_record:e2e",
        client_message_id: `cmid-${sid}-q`,
      });
      expect(user.session_id).toBe(sid);
      // Fresh session: the committed seq is positional (0-based).
      expect(user.seq).toBe(0);
      // A user record roots its own thread on its client_message_id.
      expect(user.thread_id).toBe(`cmid-${sid}-q`);

      const assistant = await client.rawRequest<AppendResult>("session/append_message", {
        session_id: sid,
        role: "assistant",
        content: "Tokyo.",
        source: "external_record:e2e",
      });
      expect(assistant.seq).toBe(1);
      // The assistant record derived its thread from the preceding user row.
      expect(assistant.thread_id).toBe(`cmid-${sid}-q`);

      // The records are durable history: the read side sees them, tags
      // included, exactly as persisted.
      const page = await client.rawRequest<{
        messages: Array<{ role: string; content: string; source?: string }>;
        has_more: boolean;
        next_offset: number;
      }>("session/messages_page", { session_id: sid });
      const appended = page.messages.filter((m) => m.source === "external_record:e2e");
      expect(appended.map((m) => `${m.role}:${m.content}`)).toEqual([
        "user:What is the capital of Japan?",
        "assistant:Tokyo.",
      ]);
    } finally {
      await client.close();
    }
  });

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
      const first = await client.rawRequest<AppendResult>("session/append_message", params);
      const retry = await client.rawRequest<AppendResult>("session/append_message", {
        ...params,
        content: "a DIFFERENT body must still lose to the first row",
      });
      expect(retry.seq).toBe(first.seq);

      const page = await client.rawRequest<{
        messages: Array<{ content: string }>;
      }>("session/messages_page", { session_id: sid });
      expect(
        page.messages.filter((m) => m.content === "importer checkpoint"),
      ).toHaveLength(1);
      expect(
        page.messages.filter((m) => m.content.includes("DIFFERENT body")),
      ).toHaveLength(0);
    } finally {
      await client.close();
    }
  });

  test("rejects an unknown role and an unbindable assistant record with invalid_params", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient(env);
    const sid = uniqueSessionId("m9-append-invalid");
    try {
      await client.openSession({ session_id: sid });

      const badRole = await expectRpcError(
        () =>
          client.rawRequest("session/append_message", {
            session_id: sid,
            role: "tool",
            content: "not a conversational record",
            source: "external_record:e2e",
          }),
        RPC_ERROR_CODES.INVALID_PARAMS,
      );
      expect(badRole.data?.kind).toBe("invalid_role");

      // A fresh session with no user row cannot take an assistant record
      // without an explicit thread binding — the server must not mint
      // threads on a public wire surface.
      const unbound = await expectRpcError(
        () =>
          client.rawRequest("session/append_message", {
            session_id: uniqueSessionId("m9-append-orphan"),
            role: "assistant",
            content: "no user row to bind to",
            source: "external_record:e2e",
          }),
        RPC_ERROR_CODES.INVALID_PARAMS,
      );
      expect(unbound.data?.kind).toBe("unbound_thread");
    } finally {
      await client.close();
    }
  });

  test("a second live connection receives the projection envelope for appended records", async () => {
    const env = liveServerEnv();
    const sid = uniqueSessionId("m9-append-live");
    const observer = new M9WsClient(env);
    const writer = new M9WsClient(env);
    try {
      // The observer opens the session FIRST so its baseline cursor precedes
      // the appends; everything the writer commits must reach it live.
      await observer.openSession({ session_id: sid });
      await writer.openSession({ session_id: sid });

      const appendPromise = writer.rawRequest<AppendResult>("session/append_message", {
        session_id: sid,
        role: "user",
        content: "live projection probe",
        source: "external_record:e2e",
        client_message_id: `cmid-${sid}-live`,
      });
      const envelope = await observer.waitForEnvelopePayload("user_message");
      expect(envelope.params).toBeTruthy();
      const appended = await appendPromise;
      expect(appended.seq).toBe(0);
    } finally {
      await writer.close();
      await observer.close();
    }
  });
});
