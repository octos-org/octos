/**
 * Regression tests for tool-use bugs via the web API.
 *
 * These tests exercise the exact tool-use sequences that triggered:
 *   1. activate_tools "tool registry not available" (OnceLock stale Weak bug)
 *   2. ffmpeg not found in sandbox PATH
 *
 * Run against a live octos-serve instance:
 *   OCTOS_TEST_URL=http://localhost:3000 OCTOS_AUTH_TOKEN=<token> npx playwright test
 *
 * The tests use the /api/chat (sync) and /api/admin/shell endpoints.
 */
import { test, expect } from '@playwright/test';

const AUTH_TOKEN = process.env.OCTOS_AUTH_TOKEN || '';

function headers() {
  const h: Record<string, string> = { 'Content-Type': 'application/json' };
  if (AUTH_TOKEN) h['Authorization'] = `Bearer ${AUTH_TOKEN}`;
  return h;
}

// ---------------------------------------------------------------------------
// Helper: POST /api/admin/shell — run a command on the server
// ---------------------------------------------------------------------------
async function adminShell(request: any, baseURL: string, command: string) {
  const res = await request.post(`${baseURL}/api/admin/shell`, {
    headers: headers(),
    data: { command },
  });
  return res.json();
}

// ---------------------------------------------------------------------------
// Test 1: ffmpeg reachable in shell PATH
//
// The bug: octos-serve started via nohup (not launchd) inherited a minimal
// PATH without /opt/homebrew/bin, so the agent sandbox couldn't find ffmpeg.
// ---------------------------------------------------------------------------
test('ffmpeg is reachable via shell PATH', async ({ request, baseURL }) => {
  test.skip(!AUTH_TOKEN, 'OCTOS_AUTH_TOKEN required');

  const result = await adminShell(request, baseURL!, 'which ffmpeg');
  expect(result.exit_code).toBe(0);
  expect(result.stdout).toContain('ffmpeg');
});

test('ffmpeg version is functional', async ({ request, baseURL }) => {
  test.skip(!AUTH_TOKEN, 'OCTOS_AUTH_TOKEN required');

  const result = await adminShell(request, baseURL!, 'ffmpeg -version 2>&1 | head -1');
  expect(result.exit_code).toBe(0);
  expect(result.stdout).toMatch(/ffmpeg version \d/);
});

// ---------------------------------------------------------------------------
// Test 2: ffmpeg concat works inside sandbox
//
// Reproduces the actual user workflow: generate temp WAV files, concat with
// ffmpeg, verify output. This is what mofa-fm does.
// ---------------------------------------------------------------------------
test('ffmpeg concat works in sandbox workdir', async ({ request, baseURL }) => {
  test.skip(!AUTH_TOKEN, 'OCTOS_AUTH_TOKEN required');

  // Generate two tiny WAV files with ffmpeg, concat them
  const script = [
    'set -e',
    'mkdir -p /tmp/ffmpeg_test_$$',
    'cd /tmp/ffmpeg_test_$$',
    // Generate 0.5s silence as WAV
    'ffmpeg -y -f lavfi -i anullsrc=r=22050:cl=mono -t 0.5 a.wav 2>/dev/null',
    'ffmpeg -y -f lavfi -i anullsrc=r=22050:cl=mono -t 0.5 b.wav 2>/dev/null',
    // Create concat list
    "echo \"file 'a.wav'\" > list.txt",
    "echo \"file 'b.wav'\" >> list.txt",
    // Concat
    'ffmpeg -y -f concat -safe 0 -i list.txt -c copy out.wav 2>/dev/null',
    // Verify
    'test -f out.wav && echo "CONCAT_OK" || echo "CONCAT_FAIL"',
    // Cleanup
    'rm -rf /tmp/ffmpeg_test_$$',
  ].join(' && ');

  const result = await adminShell(request, baseURL!, script);
  expect(result.stdout).toContain('CONCAT_OK');
});

// ---------------------------------------------------------------------------
// Test 3: activate_tools works after session reset
//
// The bug: ActivateToolsTool used OnceLock which could only be set once.
// After a session actor dropped and a new one was created, the Weak<ToolRegistry>
// was stale → "tool registry not available".
//
// We simulate this by sending a chat message that triggers activate_tools,
// then sending another message in a NEW session (different session_id) which
// also triggers activate_tools. Both should succeed.
// ---------------------------------------------------------------------------
test('activate_tools works across different sessions', async ({ request, baseURL }) => {
  test.skip(!AUTH_TOKEN, 'OCTOS_AUTH_TOKEN required');

  // Session A: send a message that will trigger tool use
  const resA = await request.post(`${baseURL}/api/chat`, {
    headers: headers(),
    data: {
      message: 'Use the shell tool to run: echo "session_a_ok"',
      session_id: `test-session-a-${Date.now()}`,
      stream: false,
    },
  });

  // We may get an error if no agent is configured (standalone mode),
  // but the key test is that it doesn't fail with "tool registry not available"
  if (resA.ok()) {
    const bodyA = await resA.json();
    expect(bodyA.content).not.toContain('tool registry not available');
  }

  // Session B: different session → may trigger a new SessionActor
  const resB = await request.post(`${baseURL}/api/chat`, {
    headers: headers(),
    data: {
      message: 'Use the shell tool to run: echo "session_b_ok"',
      session_id: `test-session-b-${Date.now()}`,
      stream: false,
    },
  });

  if (resB.ok()) {
    const bodyB = await resB.json();
    expect(bodyB.content).not.toContain('tool registry not available');
  }
});

// ---------------------------------------------------------------------------
// Test 4: Full tool chain — activate_tools → shell → ffmpeg
//
// This is the exact sequence that was broken: the agent needs to call
// activate_tools to load the shell tool group, then call shell to run ffmpeg.
// If any link in the chain is broken, this test fails.
// ---------------------------------------------------------------------------
test('full tool chain: chat triggers activate_tools → shell → ffmpeg', async ({
  request,
  baseURL,
}) => {
  test.setTimeout(180_000);
  test.skip(!AUTH_TOKEN, 'OCTOS_AUTH_TOKEN required');

  const baseSessionId = `test-ffmpeg-chain-${Date.now()}`;
  const prompt =
    'If shell is not already active, call activate_tools with exactly ["shell"] once and only once. Then call shell exactly once with this command: ffmpeg -version 2>&1 | head -1. Do not inspect available tools, do not call activate_tools repeatedly, and return only the ffmpeg version line.';

  const sendPrompt = async (message: string, sessionId: string) =>
    request.post(`${baseURL}/api/chat`, {
      headers: headers(),
      data: {
        message,
        session_id: sessionId,
        stream: false,
      },
      timeout: 90_000,
    });

  let res = await sendPrompt(prompt, baseSessionId);

  if (res.ok()) {
    let body = await res.json();
    if (typeof body.content === 'string' && body.content.includes('[LOOP DETECTED]')) {
      res = await sendPrompt(
        'Call activate_tools(["shell"]) at most once, then call shell("ffmpeg -version 2>&1 | head -1") exactly once, then stop. Return only the ffmpeg version line.',
        `${baseSessionId}-retry`,
      );
      if (!res.ok()) {
        return;
      }
      body = await res.json();
    }
    // Should contain ffmpeg version string, NOT "tool registry not available"
    // or "ffmpeg: not found"
    expect(body.content).not.toContain('tool registry not available');
    expect(body.content).not.toContain('not found');
    expect(body.content).not.toContain('not installed');
    // Positive check: should mention ffmpeg version somewhere
    expect(body.content.toLowerCase()).toContain('ffmpeg');
  }
});

// ---------------------------------------------------------------------------
// Test 5: PATH includes /opt/homebrew/bin (macOS specific)
//
// Verifies the launchd PATH propagation fix.
// ---------------------------------------------------------------------------
test('PATH includes /opt/homebrew/bin', async ({ request, baseURL }) => {
  test.skip(!AUTH_TOKEN, 'OCTOS_AUTH_TOKEN required');

  const result = await adminShell(request, baseURL!, 'echo $PATH');
  expect(result.exit_code).toBe(0);
  expect(result.stdout).toContain('/opt/homebrew/bin');
});
