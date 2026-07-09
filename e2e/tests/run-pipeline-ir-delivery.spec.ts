import { expect, test } from '@playwright/test';
import {
  freshTurnId,
  M9WsClient,
  uniqueSessionId,
  type UiNotification,
} from '../lib/m9-ws-client';

const BASE = process.env.OCTOS_TEST_URL || 'http://127.0.0.1:50123';
const TOKEN =
  process.env.OCTOS_AUTH_TOKEN ||
  process.env.OCTOS_LIVE_TOKEN ||
  process.env.OCTOS_TEST_TOKEN ||
  '';
const PROFILE = process.env.OCTOS_PROFILE || 'admin';

test.setTimeout(180_000);

function waitForMatchingNotification(
  client: M9WsClient,
  label: string,
  predicate: (notification: UiNotification) => boolean,
  timeoutMs: number,
): Promise<UiNotification> {
  const existing = client.notificationsLog().find(predicate);
  if (existing) return Promise.resolve(existing);

  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      reject(new Error(`timed out waiting for ${label}`));
    }, timeoutMs);

    client.onNotification((notification) => {
      if (!predicate(notification)) return;
      clearTimeout(timer);
      resolve(notification);
    });
  });
}

test('text-only IR run_pipeline reaches the user as a background completion', async () => {
  expect(TOKEN, 'OCTOS_AUTH_TOKEN must be set for live e2e').not.toEqual('');

  const client = new M9WsClient({
    url: BASE,
    token: TOKEN,
    profileId: PROFILE,
    requestTimeoutMs: 45_000,
    uiFeatures: [
      'event.message_persisted.v1',
      'event.spawn_complete.v1',
      'auxiliary.rest_to_ws.v1',
    ],
  });

  const sessionId = uniqueSessionId('run-pipeline-ir-delivery');
  const turnId = freshTurnId();
  const ir =
    '{"id":"ir_delivery_smoke","nodes":[{"id":"start","kind":{"type":"transform","prompt":"Return exactly DOT_UI_SMOKE_OK and no other text."}}],"edges":[]}';
  const prompt =
    'Call run_pipeline exactly once. Use input exactly DOT_UI_SMOKE_OK. ' +
    `Use this exact typed IR JSON string as the ir argument: ${ir} ` +
    'Do not answer directly and do not call any other tool.';

  try {
    await client.connect();

    const foregroundDone = waitForMatchingNotification(
      client,
      'foreground turn/completed',
      (notification) =>
        notification.method === 'turn/completed' &&
        notification.params?.turn_id === turnId,
      90_000,
    );
    const backgroundDone = waitForMatchingNotification(
      client,
      'run_pipeline turn/spawn_complete',
      (notification) =>
        notification.method === 'turn/spawn_complete' &&
        notification.params?.turn_id === turnId &&
        notification.params?.tool_call_id,
      120_000,
    );

    await client.openSession({ session_id: sessionId, profile_id: PROFILE });
    await client.startTurn({
      session_id: sessionId,
      turn_id: turnId,
      input: [{ kind: 'text', text: prompt }],
    });

    await foregroundDone;
    const background = await backgroundDone;
    const content = String(background.params?.content ?? '');
    const media = Array.isArray(background.params?.media)
      ? background.params.media.map(String)
      : [];

    console.log(
      `[run_pipeline-ir-delivery] content=${JSON.stringify(content.slice(0, 240))} media=${JSON.stringify(media)}`,
    );

    expect(content).toContain('DOT_UI_SMOKE_OK');
    expect(content).not.toMatch(/run_pipeline failed|file delivery failed/i);
    expect(
      media.some(
        (path) => path.includes('/skill-output/run_pipeline/') && path.endsWith('.md'),
      ),
    ).toBe(true);
  } finally {
    await client.close();
  }
});
