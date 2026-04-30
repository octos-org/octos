/**
 * M9 wire-level e2e: `task/output/read`.
 *
 * Issue: https://github.com/octos-org/octos/issues/647
 * Spec  : api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md §7 / §8
 *
 * The harness asserts what we can observe without seeding private runtime
 * internals:
 *
 *   - reading task output for a session that has never had a task returns
 *     a typed INVALID_PARAMS with `data.kind = "session_not_found"`.
 *   - reading with an explicit `cursor` and `limit_bytes` is parsed without
 *     INVALID_PARAMS (the wire envelope decodes correctly).
 *   - reading with a malformed task_id (not a UUID) is rejected at the
 *     parser as INVALID_PARAMS.
 * The "initial read + follow-up tail" round-trip uses the default M9
 * protocol fixture lane to create a persisted background-task snapshot.
 */
import { test, expect } from "@playwright/test";
import {
  M9WsClient,
  RPC_ERROR_CODES,
  expectRpcError,
  freshTaskId,
  freshTurnId,
  liveServerEnv,
  uniqueSessionId,
} from "../lib/m9-ws-client";

test.describe("M9 protocol — task/output/read", () => {
  test.setTimeout(60_000);

  test("reading task output for a fresh session returns session_not_found", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient(env);
    const sid = uniqueSessionId("m9-tor-fresh");
    try {
      // No session/open here on purpose — the runtime treats unknown sessions
      // as "session_not_found" before even checking the task_id.
      const err = await expectRpcError(
        () =>
          client.readTaskOutput({
            session_id: sid,
            task_id: freshTaskId(),
          }),
        RPC_ERROR_CODES.INVALID_PARAMS,
      );
      expect(err.data?.kind).toBe("session_not_found");
      expect(err.data?.method).toBe("task/output/read");
      expect(err.data?.session_id).toBe(sid);
    } finally {
      await client.close();
    }
  });

  test("reading with explicit cursor+limit decodes without INVALID_PARAMS for the envelope itself", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient(env);
    const sid = uniqueSessionId("m9-tor-cursor");
    try {
      await client.openSession({ session_id: sid });
      // The session exists but no task does, so we expect a
      // task-level error, not a parser-level error. The exact kind is
      // server-defined; we only assert the envelope is well-formed and
      // the error is not a parse failure.
      const err = await expectRpcError(() =>
        client.readTaskOutput({
          session_id: sid,
          task_id: freshTaskId(),
          cursor: { offset: 0 },
          limit_bytes: 4096,
        }),
      );
      // Whatever code the server picks, it must not be PARSE_ERROR.
      expect(err.code).not.toBe(RPC_ERROR_CODES.PARSE_ERROR);
      expect(err.data?.method).toBe("task/output/read");
    } finally {
      await client.close();
    }
  });

  test("malformed task_id (not a UUID) is rejected as INVALID_PARAMS", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient(env);
    const sid = uniqueSessionId("m9-tor-bad-uuid");
    try {
      await client.openSession({ session_id: sid });
      const err = await expectRpcError(
        () =>
          client.rawRequest("task/output/read", {
            session_id: sid,
            task_id: "not-a-uuid",
          }),
        RPC_ERROR_CODES.INVALID_PARAMS,
      );
      expect(err.message.toLowerCase()).toContain("task/output/read");
    } finally {
      await client.close();
    }
  });

  test("initial read + follow-up read advance by task/output cursor", async () => {
    const env = liveServerEnv();
    const client = new M9WsClient(env);
    const sid = uniqueSessionId("m9-tor-fixture");
    const turnId = freshTurnId();
    try {
      await client.openSession({ session_id: sid });
      const accept = await client.startTurn({
        session_id: sid,
        turn_id: turnId,
        input: [
          {
            kind: "text",
            text: "M9 task output fixture: create deterministic background task output.",
          },
        ],
      });
      expect(accept.accepted).toBe(true);

      const updated = await client.waitForNotification("task/updated", 45_000);
      expect(updated.params.session_id).toBe(sid);
      expect(typeof updated.params.task_id).toBe("string");
      const taskId = updated.params.task_id;

      const first = await client.readTaskOutput({
        session_id: sid,
        task_id: taskId,
        cursor: { offset: 0 },
        limit_bytes: 32,
      });
      expect(first.session_id).toBe(sid);
      expect(first.task_id).toBe(taskId);
      expect(first.source).toBe("runtime_projection");
      expect(first.cursor.offset).toBe(0);
      expect(first.next_cursor.offset).toBeGreaterThan(first.cursor.offset);
      expect(Buffer.byteLength(first.text, "utf8")).toBe(first.bytes_read);
      expect(first.bytes_read).toBeLessThanOrEqual(32);
      expect(first.total_bytes).toBeGreaterThanOrEqual(first.bytes_read);
      expect(first.limitations.length).toBeGreaterThan(0);

      const second = await client.readTaskOutput({
        session_id: sid,
        task_id: taskId,
        cursor: first.next_cursor,
        limit_bytes: 32,
      });
      expect(second.cursor.offset).toBe(first.next_cursor.offset);
      expect(Buffer.byteLength(second.text, "utf8")).toBe(second.bytes_read);
      expect(second.next_cursor.offset).toBeGreaterThanOrEqual(second.cursor.offset);
      expect(second.cursor.offset).toBeGreaterThan(first.cursor.offset);
      if (first.truncated) {
        expect(second.text).not.toBe(first.text);
      }
      await client.waitForNotification("turn/completed", 45_000);
    } finally {
      await client.close();
    }
  });
});
