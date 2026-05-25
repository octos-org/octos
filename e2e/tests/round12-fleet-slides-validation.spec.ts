/**
 * Round-12 fleet slides validator v2 — P0-A + P0-B closure
 *
 * P0-A: server emits `file/attached` WS envelope with a `.pptx` path AND the
 *       web client renders a clickable pptx download button at completion.
 * P0-B: mini1 (crew) routes deck prompts through `mofa_slides`, not
 *       `mofa_site` / `mofa_youtube`.
 *
 * Runs the same short slides prompt against all four fleet hosts in
 * parallel, captures every JSON WebSocket frame, and writes a per-host
 * JSON report (plus a final screenshot) under
 * `e2e/test-results-round12-slides/<host>/`.
 *
 * v2 corrections (vs first run that returned 3/4 hosts as "never invoked
 * `mofa_slides`"):
 *   - Session creation now uses `/new slides round12-quasars` as the FIRST
 *     chat input so the server-side session topic is seeded with
 *     `slides:round12-quasars`. PR #1265's per-session allowlist only
 *     retains `mofa_slides` (and evicts every sibling `mofa_*`) when the
 *     session topic starts with `slides`. The v1 run used a generic
 *     `createNewSession(page)` (no topic) → the allowlist never engaged
 *     → 3/4 hosts saw zero `mofa_slides` invocations.
 *   - The WebSocket `framereceived` listener is now attached BEFORE the
 *     first user message is sent so the slash-command-emitted frames are
 *     captured (fixes the ocean WS-sniffer-attach race).
 *
 * One-shot validator. Cap wall clock per host: 6 minutes for completion +
 * the usual login/session setup overhead.
 */
import { expect, test, type Page } from '@playwright/test';
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import {
  countAssistantBubbles,
  createNewSession,
  getInput,
  getSendButton,
  login,
  SEL,
} from './live-browser-helpers';

interface HostSpec {
  /** Short label used for the result directory + log output. */
  key: 'crew' | 'bot' | 'octos' | 'ocean';
  /** Full base URL (must include scheme + host, no trailing slash). */
  baseUrl: string;
  /** Friendly host string for the report ("crew.ominix.io"). */
  displayHost: string;
}

const HOSTS: HostSpec[] = [
  { key: 'crew', baseUrl: 'https://dspfac.crew.ominix.io', displayHost: 'crew.ominix.io' },
  { key: 'bot', baseUrl: 'https://dspfac.bot.ominix.io', displayHost: 'bot.ominix.io' },
  { key: 'octos', baseUrl: 'https://dspfac.octos.ominix.io', displayHost: 'octos.ominix.io' },
  { key: 'ocean', baseUrl: 'https://dspfac.ocean.ominix.io', displayHost: 'ocean.ominix.io' },
];

// Round-13 v2 update: the slides skill is two-phase (outline first, then
// `generate`/`go` to invoke `mofa_slides`). v2 sent only this PROMPT and
// the validator never saw `mofa_slides` because the model stopped after
// proposing the outline. Now we explicitly request "outline only" and add
// a follow-up `generate`/`go` phase below.
const PROMPT =
  'Make a 2-slide intro deck about quasars — title + 1 content. Show the outline only. Do not generate yet — wait for me to reply "generate" or "go".';
// Sent after the assistant returns the outline (or after a short delay if
// the outline phase stalls). Triggers the actual `mofa_slides` invocation.
const GENERATE_TRIGGERS = ['generate', 'go'] as const;
// Slug fed to `/new slides <slug>`. Keep it short, no spaces — the server
// stores this as the session topic (`slides:<slug>`) which is what PR
// #1265's per-session allowlist checks via `topic.starts_with("slides")`.
const SLIDES_SLUG = 'round12-quasars';
const NEW_SLIDES_CMD = `/new slides ${SLIDES_SLUG}`;
const COMPLETION_TIMEOUT_MS = 6 * 60 * 1000; // 6 min cap per host
// Cap on how long we wait for the slides scaffold to ack after sending
// `/new slides …`. The scaffold ack on a healthy mini lands in <10s; we
// budget 60s to absorb a slow mini1 round-trip but no more, so the test
// gives up early on a wedged host rather than burning the per-host cap.
const SCAFFOLD_ACK_TIMEOUT_MS = 60_000;
const RESULTS_ROOT = join(
  process.cwd(),
  'e2e',
  'test-results-round12-slides',
);
// The test runner is launched from inside `e2e/`, so `process.cwd()` ends
// in `/e2e`. Strip the trailing `/e2e/e2e` collision if the runner is
// elsewhere.
const RESULTS_ROOT_RESOLVED = RESULTS_ROOT.endsWith('/e2e/e2e/test-results-round12-slides')
  ? RESULTS_ROOT.replace('/e2e/e2e/', '/e2e/')
  : RESULTS_ROOT;

interface CapturedFrame {
  /** Wall-clock ms since test start. */
  t: number;
  /** Parsed JSON-RPC method (if any). */
  method: string | null;
  /** Parsed JSON-RPC `params.tool_name` (for tool/* events). */
  tool_name: string | null;
  /** Parsed JSON-RPC `params.path` (for file/attached). */
  path: string | null;
}

interface HostResult {
  host: string;
  baseUrl: string;
  completedAtMs: number | null;
  timedOut: boolean;
  fileAttachedEvents: number;
  pptxAttachedPath: string | null;
  domButtonCount: number;
  toolCallsSeen: string[];
  assistantBubbleCount: number;
  taskDockCount: number | null;
  consoleErrorCount: number;
  framesTotal: number;
  // Tracked for debugging when nothing fires.
  uniqueMethodsSeen: string[];
  errors: string[];
}

function tryParseFrame(payload: unknown): {
  method: string | null;
  tool_name: string | null;
  path: string | null;
} {
  if (typeof payload !== 'string') {
    return { method: null, tool_name: null, path: null };
  }
  // Cheap pre-filter: skip ping/heartbeat which the server sends every
  // few seconds. They aren't JSON-RPC envelopes we care about.
  if (payload.length === 0) {
    return { method: null, tool_name: null, path: null };
  }
  try {
    const obj = JSON.parse(payload) as {
      method?: unknown;
      params?: { tool_name?: unknown; path?: unknown };
    };
    const method = typeof obj.method === 'string' ? obj.method : null;
    const tool_name =
      typeof obj.params?.tool_name === 'string' ? obj.params.tool_name : null;
    const path = typeof obj.params?.path === 'string' ? obj.params.path : null;
    return { method, tool_name, path };
  } catch {
    return { method: null, tool_name: null, path: null };
  }
}

/**
 * DOM selectors that the web client uses (any of these counts as a pptx
 * download affordance). The dashboard renders `*.pptx` as a button with
 * the filename in `aria-label`/text; older bundles may have only an
 * anchor with `href` or `download`. Be permissive: P0-A WEB asserts the
 * USER can click something to grab the deck.
 */
const PPTX_BUTTON_LOCATOR =
  'a[href*=".pptx"], a[download*=".pptx"], button:has-text(".pptx"), [data-testid*="file"][data-testid*="pptx"], [data-testid*="attachment"]';

async function countPptxButtons(page: Page): Promise<number> {
  return page.locator(PPTX_BUTTON_LOCATOR).count().catch(() => 0);
}

async function readTaskDockCount(page: Page): Promise<number | null> {
  // Tolerate selector drift across bundles.
  const candidates = [
    '[data-testid="task-dock-count"]',
    '[data-testid="task-dock"] [data-testid$="count"]',
    '[data-testid="task-dock"] .badge',
  ];
  for (const sel of candidates) {
    const node = page.locator(sel).first();
    const visible = await node.isVisible().catch(() => false);
    if (!visible) continue;
    const text = ((await node.textContent().catch(() => '')) || '').trim();
    const m = text.match(/(\d+)/);
    if (m) return Number(m[1]);
  }
  return null;
}

async function sendPrompt(page: Page, message: string): Promise<void> {
  const input = getInput(page);
  const send = getSendButton(page);
  await input.fill(message);
  await send.click();
}

async function runOneHost(host: HostSpec, page: Page): Promise<HostResult> {
  const t0 = Date.now();
  const frames: CapturedFrame[] = [];
  const consoleErrors: string[] = [];

  // Attach the WebSocket framereceived listener BEFORE any navigation so
  // we don't miss the early frames the server emits during session-open
  // and `/new slides …` scaffold dispatch. The v1 ocean run hit this race
  // (the WS opened before the listener was wired) and reported zero
  // `tool/started` frames despite the server having emitted them.
  page.on('websocket', (ws) => {
    ws.on('framereceived', (e) => {
      const parsed = tryParseFrame(e.payload);
      // Skip the (very frequent) keepalive frames: they have no `method`
      // field, so we'd just inflate the buffer with nulls.
      if (!parsed.method) return;
      frames.push({ t: Date.now() - t0, ...parsed });
    });
  });

  page.on('console', (msg) => {
    if (msg.type() === 'error') {
      consoleErrors.push(msg.text().slice(0, 200));
    }
  });

  await login(page);
  await createNewSession(page);

  // Seed the session topic to `slides:round12-quasars` via the slash
  // command so PR #1265's per-session `mofa_*` allowlist engages. With
  // this in place, mini1's tool registry retains only `mofa_slides`
  // (siblings `mofa_site`/`mofa_youtube`/etc are structurally evicted)
  // for the rest of the session, regardless of model judgement.
  await sendPrompt(page, NEW_SLIDES_CMD);

  // Wait for the scaffold ack to land before sending the real prompt.
  // The slides scaffold emits a `tool/started` for the scaffold step
  // followed by an assistant bubble with the project layout. We treat
  // "session has at least one tool frame OR a second assistant bubble"
  // as good-enough. If we never see either within
  // SCAFFOLD_ACK_TIMEOUT_MS we still proceed — the per-host completion
  // cap will surface the wedge as a timeout in the report.
  const scaffoldDeadline = Date.now() + SCAFFOLD_ACK_TIMEOUT_MS;
  while (Date.now() < scaffoldDeadline) {
    const anyToolFrame = frames.some(
      (f) => f.method === 'tool/started' || f.method === 'tool/completed',
    );
    const bubbleCount = await countAssistantBubbles(page).catch(() => 0);
    if (anyToolFrame || bubbleCount >= 1) break;
    await page.waitForTimeout(1_500);
  }

  // Now send the real deck prompt. The session topic is already seeded,
  // so this turn's tool resolution runs against the slides-filtered
  // registry.
  await sendPrompt(page, PROMPT);

  // Round-13 v2 fix: wait for the outline phase to settle, then send the
  // confirmation triggers ("generate", "go") to drive the model into the
  // mofa_slides invocation. Without these triggers, v2 saw the outline
  // bubble + `mofa_list_styles` etc., but never saw `mofa_slides` itself
  // because the assistant stopped awaiting confirmation.
  //
  // Strategy:
  //   1. Wait up to 90s for the assistant bubble count to grow OR a
  //      `turn/completed` to fire (outline turn done).
  //   2. Send "generate".
  //   3. Wait up to 30s more, send "go" as belt+suspenders. The slides
  //      flow tolerates extra "go" — see live-slides-site.spec.ts:316.
  const outlineDeadline = Date.now() + 90_000;
  const bubbleBaseline = await countAssistantBubbles(page).catch(() => 0);
  while (Date.now() < outlineDeadline) {
    const bubbleNow = await countAssistantBubbles(page).catch(() => 0);
    const completed = frames.some((f) => f.method === 'turn/completed');
    if (bubbleNow > bubbleBaseline || completed) break;
    await page.waitForTimeout(3_000);
  }
  for (const trigger of GENERATE_TRIGGERS) {
    await sendPrompt(page, trigger).catch(() => undefined);
    // Brief delay so each trigger has a chance to drive a fresh turn
    // before the next "go" is queued.
    await page.waitForTimeout(15_000);
  }

  // Wait for completion: EITHER a `file/attached` frame whose path ends
  // in `.pptx`, OR a DOM pptx button. Poll every 3s up to 6 min.
  const deadline = Date.now() + COMPLETION_TIMEOUT_MS;
  let timedOut = false;
  let completedAtMs: number | null = null;

  while (Date.now() < deadline) {
    const wsHit = frames.find(
      (f) =>
        f.method === 'file/attached' &&
        typeof f.path === 'string' &&
        f.path.toLowerCase().endsWith('.pptx'),
    );
    if (wsHit) {
      completedAtMs = wsHit.t;
      break;
    }
    const domButtons = await countPptxButtons(page);
    if (domButtons > 0) {
      completedAtMs = Date.now() - t0;
      break;
    }
    await page.waitForTimeout(3_000);
  }
  if (completedAtMs === null) {
    timedOut = true;
  }

  const fileAttachedFrames = frames.filter((f) => f.method === 'file/attached');
  const pptxAttachedFrame = fileAttachedFrames.find(
    (f) => typeof f.path === 'string' && f.path.toLowerCase().endsWith('.pptx'),
  );
  const toolStarted = frames.filter((f) => f.method === 'tool/started');
  const toolCallsSeen = Array.from(
    new Set(
      toolStarted
        .map((f) => f.tool_name)
        .filter((n): n is string => typeof n === 'string'),
    ),
  );
  const uniqueMethodsSeen = Array.from(
    new Set(frames.map((f) => f.method).filter((m): m is string => !!m)),
  );

  const domButtonCount = await countPptxButtons(page);
  const assistantBubbleCount = await countAssistantBubbles(page);
  const taskDockCount = await readTaskDockCount(page);

  const result: HostResult = {
    host: host.displayHost,
    baseUrl: host.baseUrl,
    completedAtMs,
    timedOut,
    fileAttachedEvents: fileAttachedFrames.length,
    pptxAttachedPath: pptxAttachedFrame?.path ?? null,
    domButtonCount,
    toolCallsSeen,
    assistantBubbleCount,
    taskDockCount,
    consoleErrorCount: consoleErrors.length,
    framesTotal: frames.length,
    uniqueMethodsSeen,
    errors: consoleErrors.slice(0, 20),
  };

  const outDir = join(RESULTS_ROOT_RESOLVED, host.key);
  mkdirSync(outDir, { recursive: true });
  writeFileSync(
    join(outDir, 'result.json'),
    JSON.stringify(result, null, 2),
    'utf-8',
  );
  await page
    .screenshot({ path: join(outDir, 'screenshot-final.png'), fullPage: true })
    .catch(() => undefined);

  return result;
}

test.describe.parallel('Round-12 fleet slides validation v2', () => {
  test.setTimeout(8 * 60_000); // 8 min per host (6 min wait + setup overhead)

  for (const host of HOSTS) {
    test.describe(`host=${host.displayHost}`, () => {
      test.use({ baseURL: host.baseUrl });

      test(`[${host.key}] slides round-12 P0 closure`, async ({ page }) => {
        const result = await runOneHost(host, page);

        // ---- Assertions: P0-A SERVER (file/attached) -----------------
        expect.soft(result.fileAttachedEvents, 'P0-A SERVER: file/attached frame count').toBeGreaterThanOrEqual(1);
        expect
          .soft(result.pptxAttachedPath, 'P0-A SERVER: pptx path on file/attached')
          .not.toBeNull();
        if (result.pptxAttachedPath) {
          expect
            .soft(
              result.pptxAttachedPath.toLowerCase().endsWith('.pptx'),
              `P0-A SERVER: path ends in .pptx (got "${result.pptxAttachedPath}")`,
            )
            .toBe(true);
        }

        // ---- Assertions: P0-A WEB (DOM button) -----------------------
        expect.soft(result.domButtonCount, 'P0-A WEB: DOM pptx download element count').toBeGreaterThanOrEqual(1);

        // ---- Assertions: P0-B (mini1 routing) ------------------------
        if (host.key === 'crew') {
          expect
            .soft(
              result.toolCallsSeen.includes('mofa_slides'),
              `P0-B mini1: tool/started must include mofa_slides (saw: ${JSON.stringify(result.toolCallsSeen)})`,
            )
            .toBe(true);
          expect
            .soft(
              result.toolCallsSeen.includes('mofa_site'),
              `P0-B mini1: tool/started must NOT include mofa_site`,
            )
            .toBe(false);
          expect
            .soft(
              result.toolCallsSeen.includes('mofa_youtube'),
              `P0-B mini1: tool/started must NOT include mofa_youtube`,
            )
            .toBe(false);
        }
      });
    });
  }
});
