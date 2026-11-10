/**
 * M8.10 stress test: thread_id binding under realistic 3-10 user overflow.
 *
 * Real users (issue #649) send several messages with random gaps and at
 * least one slow tool. The retained media-mix scenario puts sustained
 * pressure on the sticky map across five different spawn_only skills.
 *
 * The `media-mix-soak-five-skills` scenario is the canonical soaking
 * test — 5 different spawn_only skills (builtin-voice TTS, cloned-voice
 * TTS, news-digest + TTS, deep research, FM podcast) all running in
 * parallel. Each finalises asynchronously, putting sustained pressure
 * on the sticky-map. This mirrors the realistic "user opens the app
 * and tries many features at once" load.
 *
 * Each scenario sends multiple messages within a short window, waits for
 * all responses to land, then verifies that the assistant response BELOW
 * each user bubble in DOM order matches the prompt by content.
 *
 * Required env:
 *   OCTOS_TEST_URL=https://dspfac.octos.ominix.io   (mini3, pre-#649)
 *   OCTOS_AUTH_TOKEN=octos-admin-2026
 *   OCTOS_PROFILE=dspfac
 *
 * The new thread-by-cmid renderer is gated by
 * `localStorage.octos_thread_store_v2 = '1'`.
 *
 * NEVER point at mini5 — that host is reserved for coding-green tests.
 *
 * EXPECTED behavior:
 *   - On daemons without the #649 fix, late-arriving slow tool results can
 *     bind to the wrong thread.
 *   - After #649 deploys, the media-mix scenario passes as the regression
 *     check for multi-skill overflow.
 *
 * Tracking: M8.10 follow-up issue #654 (generative property tests).
 */

import { expect, test, type Page } from "@playwright/test";

import {
  SEL,
  countAssistantBubbles,
  countUserBubbles,
  createNewSession,
  getInput,
  getSendButton,
  isPlaceholderOnly,
  isSpawnAckOnly,
  login,
  normalizeBubbleText,
} from "./live-browser-helpers";

const FLAG_KEY = "octos_thread_store_v2";

interface ScenarioMessage {
  gap_ms: number;
  text: string;
  expected_in_response: string[];
  /**
   * True when the prompt is expected to route through a `spawn_only` flow
   * (deep_research, run_pipeline, voice TTS, MoFA podcast/slides/sites).
   * Such turns emit a SPAWN-ACK as the first assistant bubble (~1-3s)
   * and the actual RESULT lands minutes later — possibly as a `.md` /
   * `.mp3` / `.pptx` attachment whose `innerText` does not contain
   * `expected_in_response` markers. Harness predicates accept either
   * the marker text OR a same-origin attachment href as evidence the
   * result has bound to this user's thread (mirrors the #649 / #731
   * attachment-aware hardening).
   */
  is_spawn_only?: boolean;
}

interface Scenario {
  name: string;
  messages: ScenarioMessage[];
  timeout_ms: number;
  /**
   * Optional override: skip the scenario with a `test.fixme` while a
   * real server/SPA bug is being fixed upstream. The string is the
   * tracking issue/PR (e.g. `#740`). Documented on each suppression so
   * future readers can see the rationale at the call-site.
   */
  fixme_pending?: string;
}

const SCENARIOS: Scenario[] = [
  {
    // Soaking test: 5 different media-producing skills running in
    // parallel. Each is spawn_only and finalises asynchronously,
    // putting sustained pressure on the sticky-map across multiple
    // thread rotations for the duration of the slowest tool
    // (typically deep_research at 3-5 min). Mirrors the canonical
    // "real user trying many features at once" load — builtin-voice
    // TTS, cloned-voice TTS, news-digest + TTS, deep research, and
    // an FM-style podcast.
    //
    // Pre-#649: at least one bubble orphans because late-arriving
    // spawn_only results collide with the rotated sticky-map.
    // Post-fix: all five thread_ids are stamped at spawn time, so
    // late finalises bind correctly regardless of which slot the
    // sticky-map currently holds.
    name: "media-mix-soak-five-skills",
    messages: [
      {
        gap_ms: 0,
        text: "用 vivian 说一段：今天是个好日子，让我们开始吧",
        expected_in_response: [
          "vivian",
          "audio",
          ".wav",
          ".mp3",
          "语音",
          "好日子",
        ],
        is_spawn_only: true,
      },
      {
        gap_ms: 8000,
        text: "用 yangmi 念这段：我是你的数字助理",
        expected_in_response: [
          "yangmi",
          "audio",
          ".wav",
          ".mp3",
          "数字",
          "助理",
        ],
        is_spawn_only: true,
      },
      {
        gap_ms: 6000,
        text: "总结一下今日科技新闻并用 vivian 朗读 (news digest + tts)",
        expected_in_response: ["news", "新闻", "tech", "科技", "audio", ".mp3"],
        is_spawn_only: true,
      },
      {
        gap_ms: 5000,
        text: "深度搜索今日Rust语言进展 (deep search)",
        expected_in_response: ["rust", "language", "语言", "news"],
        is_spawn_only: true,
      },
      {
        gap_ms: 5000,
        text: "做一个关于AI智能体平台的FM播客 (mofa podcast)",
        expected_in_response: [
          "podcast",
          "audio",
          ".mp3",
          "episode",
          "播客",
          "智能体",
        ],
        is_spawn_only: true,
      },
    ],
    timeout_ms: 900_000,
  },
];

async function enableThreadStoreV2(page: Page) {
  await page.addInitScript((key) => {
    localStorage.setItem(key, "1");
  }, FLAG_KEY);
}

interface OrderedBubble {
  index: number;
  role: "user" | "assistant";
  threadId: string;
  text: string;
  /**
   * ARTIFACT anchor `href` values inside the bubble. Spawn_only
   * deliveries (deep_research, mofa-podcast, mofa-slides, ...) finalise
   * as media attachments rather than inline text — the only DOM signal
   * that the result has bound to the user thread is an `<a href>`
   * pointing at a same-origin artifact URL. We restrict the filter to:
   *   - `/api/files?path=…` (the SPA's audio/file attachment route)
   *   - `/preview/…` (the SPA's site-preview iframe wrapper)
   *   - any anchor with the `download` attribute set (the SPA always
   *     sets this for downloadable artifacts)
   *   - URLs whose extension is in a known artifact set (`.md`, `.mp3`,
   *     `.wav`, `.ogg`, `.m4a`, `.pptx`, `.html`)
   * External citation links the LLM may emit in synthesis prose, and
   * generic same-origin chrome links (`/login`, `/admin`, …) are
   * filtered out so they don't false-pass the spawn_only attestation.
   * Per codex review on 2026-05-01 — same-origin alone was too loose.
   */
  hrefs: string[];
}

async function getOrderedBubbles(page: Page): Promise<OrderedBubble[]> {
  return page.evaluate(() => {
    const here = window.location.origin;
    // Per codex review on 2026-05-01: include `/api/preview/` (the
    // backend route the SPA actually uses) alongside `/preview/`
    // (legacy / SPA-side wrapper). Missing the `/api/` variant would
    // false-negative every mofa-sites delivery.
    const ARTIFACT_PATH_PREFIXES = ["/api/files", "/api/preview/", "/preview/"];
    const ARTIFACT_EXTENSIONS = [
      ".md",
      ".mp3",
      ".wav",
      ".ogg",
      ".m4a",
      ".pptx",
      ".html",
      ".pdf",
    ];
    const nodes = document.querySelectorAll(
      "[data-testid='user-message'], [data-testid='assistant-message']",
    );
    return Array.from(nodes).map((el, i) => ({
      index: i,
      role:
        el.getAttribute("data-testid") === "user-message"
          ? ("user" as const)
          : ("assistant" as const),
      threadId: el.getAttribute("data-thread-id") || "",
      text: ((el as HTMLElement).innerText || "").trim(),
      hrefs: Array.from(el.querySelectorAll("a[href]"))
        .filter((a) => {
          const anchor = a as HTMLAnchorElement;
          const href = anchor.href || "";
          if (!href) return false;
          let url: URL;
          try {
            url = new URL(href, here);
          } catch {
            return false;
          }
          if (url.origin !== here) return false;
          // Bare `download` attribute (no value) MUST be treated as
          // an artifact link — the SPA's audio attachments use
          // `<a download="filename.mp3">` AND `<a download>` shapes
          // depending on the renderer build. `el.download` returns
          // `''` (falsy) for the bare attribute; check via
          // `hasAttribute` per codex review on 2026-05-01.
          if (anchor.hasAttribute("download")) return true;
          const path = url.pathname;
          if (
            ARTIFACT_PATH_PREFIXES.some((prefix) => path.startsWith(prefix))
          ) {
            return true;
          }
          const lower = path.toLowerCase();
          if (
            ARTIFACT_EXTENSIONS.some((ext) => {
              if (lower.endsWith(ext)) return true;
              // `?path=…/foo.mp3` style URLs come through with the
              // extension in the search param, not the pathname.
              return url.search.toLowerCase().includes(ext);
            })
          ) {
            return true;
          }
          return false;
        })
        .map((a) => (a as HTMLAnchorElement).href || ""),
    }));
  });
}

/**
 * True iff the bubble's text or attachments prove a REAL response landed
 * (not just a "Thinking…" placeholder, not just a timestamp, not just a
 * spawn-only ack). Spawn-only turns are accepted on either content text
 * OR a same-origin attachment href — mirrors the #649 + #731
 * attachment-aware hardening.
 */
function bubbleHasRealContent(bubble: OrderedBubble): boolean {
  if (bubble.role !== "assistant") return false;
  if (bubble.hrefs.length > 0) return true;
  if (isPlaceholderOnly(bubble.text)) return false;
  if (isSpawnAckOnly(bubble.text)) return false;
  return normalizeBubbleText(bubble.text).length > 0;
}

/**
 * Wait until every user prompt in the scenario has a paired assistant
 * bubble whose body proves a REAL response (not chrome, not a
 * spawn-ack). Polls with the same `data-testid='user-message'` /
 * `assistant-message` cadence as the legacy implementation, but the
 * "filled" predicate now uses `bubbleHasRealContent` so the wait can't
 * exit on placeholder + ack chrome alone.
 *
 * Returns the count of paired-real-content bubbles when all
 * `scenario.messages` are satisfied, or the last observed count on
 * timeout. Caller compares against `scenario.messages.length` to
 * decide pass/fail.
 */
async function waitForAllAssistantsRealContent(
  page: Page,
  scenario: Scenario,
  baseUserBubbles: number,
  baseAssistantBubbles: number,
  maxWaitMs: number,
): Promise<number> {
  const start = Date.now();
  let lastFilled = -1;
  let lastStreaming = true;
  let stable = 0;
  const expected = scenario.messages.length;
  while (Date.now() - start < maxWaitMs) {
    const isStreaming = await page
      .locator(SEL.cancelButton)
      .isVisible()
      .catch(() => false);
    const allBubbles = await getOrderedBubbles(page);
    const newBubbles = allBubbles.slice(baseUserBubbles + baseAssistantBubbles);

    // Walk in order, pair each user with the FIRST real-content
    // assistant bubble between this user and the next. A scenario with
    // N user prompts is satisfied when N such pairs exist.
    const userIndices: number[] = [];
    for (let i = 0; i < newBubbles.length; i++) {
      if (newBubbles[i].role === "user") userIndices.push(i);
    }
    let pairedReal = 0;
    for (let k = 0; k < expected; k++) {
      const userIdx = userIndices[k];
      if (userIdx === undefined) break;
      const nextUserIdx =
        k + 1 < userIndices.length ? userIndices[k + 1] : newBubbles.length;
      let found = false;
      for (let j = userIdx + 1; j < nextUserIdx; j++) {
        if (bubbleHasRealContent(newBubbles[j])) {
          found = true;
          break;
        }
      }
      if (!found) break;
      pairedReal += 1;
    }

    if (pairedReal >= expected && !isStreaming) {
      stable += 1;
      if (stable >= 2) return pairedReal;
    } else {
      stable = 0;
    }

    if (pairedReal !== lastFilled || isStreaming !== lastStreaming) {
      const elapsed = ((Date.now() - start) / 1000).toFixed(0);
      console.log(
        `  [${scenario.name}] ${elapsed}s: realContent=${pairedReal}/${expected} streaming=${isStreaming}`,
      );
      lastFilled = pairedReal;
      lastStreaming = isStreaming;
    }
    await page.waitForTimeout(3_000);
  }
  return lastFilled < 0 ? 0 : lastFilled;
}

function assertPairing(
  scenario: Scenario,
  newBubbles: OrderedBubble[],
): { violations: string[]; debug: string } {
  const violations: string[] = [];

  // Walk the bubble list, picking out user bubbles in order.
  const userIndices: number[] = [];
  for (let i = 0; i < newBubbles.length; i++) {
    if (newBubbles[i].role === "user") userIndices.push(i);
  }

  if (userIndices.length < scenario.messages.length) {
    violations.push(
      `Expected ${scenario.messages.length} user bubbles, found ${userIndices.length}`,
    );
  }

  // Track thread-id collisions: each user bubble's paired assistant should
  // have a unique thread-id (when the renderer exposes it).
  const seenThreadIds = new Set<string>();

  for (let i = 0; i < scenario.messages.length; i++) {
    const userIdx = userIndices[i];
    if (userIdx === undefined) {
      violations.push(`User bubble ${i} not found in DOM`);
      continue;
    }
    const userBubble = newBubbles[userIdx];
    const message = scenario.messages[i];
    const userPrompt = message.text;

    // Look at substrings to handle whitespace / formatting changes.
    const promptShort = userPrompt
      .slice(0, 12)
      .replace(/\s+/g, "")
      .toLowerCase();
    const userTextNorm = userBubble.text.replace(/\s+/g, "").toLowerCase();
    if (!userTextNorm.includes(promptShort.slice(0, 6))) {
      violations.push(
        `User bubble ${i} text "${userBubble.text.slice(0, 60)}" doesn't contain prompt prefix "${userPrompt.slice(0, 30)}"`,
      );
    }

    // Pairing scan: collect EVERY assistant bubble between this user and
    // the next user. Spawn_only turns may have 2 bubbles (ack + result);
    // we count the FIRST one as the canonical pair for thread-id
    // uniqueness, and union all of them for the content-marker check.
    const nextUserIdx =
      i + 1 < userIndices.length ? userIndices[i + 1] : newBubbles.length;
    const between = newBubbles.slice(userIdx + 1, nextUserIdx);
    const assistantsBetween = between.filter((b) => b.role === "assistant");
    const realContentBetween = assistantsBetween.filter((b) =>
      bubbleHasRealContent(b),
    );

    if (assistantsBetween.length === 0) {
      violations.push(
        `User ${i} ("${userPrompt.slice(0, 30)}"): no assistant bubble found between this user and the next user`,
      );
      continue;
    }

    // Track thread-id uniqueness when available — read from the first
    // assistant bubble in the pair (the SPA stamps the same thread_id
    // on every assistant bubble in a turn, so first-vs-last doesn't
    // matter).
    const primaryAssistant = assistantsBetween[0];
    if (primaryAssistant.threadId) {
      if (seenThreadIds.has(primaryAssistant.threadId)) {
        violations.push(
          `User ${i}: assistant thread_id "${primaryAssistant.threadId}" already used by an earlier thread (binding collision)`,
        );
      }
      seenThreadIds.add(primaryAssistant.threadId);
    }

    // Content-match check: at least one expected marker must appear in
    // the union of all assistant text/href bodies in this turn. For
    // spawn_only turns we additionally accept a same-origin attachment
    // href (`.md`, `.mp3`, `.pptx`, `/preview/...`) as evidence the
    // result has bound to this user — the inline text for an
    // attachment-only message is often a generic
    // "✓ <tool> completed (...)" with no markers.
    const unionText = realContentBetween
      .map((b) => b.text)
      .join("\n")
      .toLowerCase();
    const unionHrefs = realContentBetween.flatMap((b) => b.hrefs);
    const markers = message.expected_in_response;
    const textMatched = markers.some((mark) =>
      unionText.includes(mark.toLowerCase()),
    );
    const hrefMatched = unionHrefs.some((href) =>
      markers.some((mark) => href.toLowerCase().includes(mark.toLowerCase())),
    );
    const spawnAttestation =
      message.is_spawn_only === true && unionHrefs.length > 0;
    const matched = textMatched || hrefMatched || spawnAttestation;

    // PHANTOM BUBBLE assertion (added 2026-05-09 after the
    // `appendPersistedMessage` empty-late-event bug shipped under the
    // old harness): if there are assistant bubbles in this turn region
    // that contain NO text (after normalization) AND NO attachment
    // hrefs, they are pure-orphan timestamp-only bubbles and constitute
    // a real production UX defect — the user sees a phantom bubble
    // appear after their question completes. The previous
    // `bubbleHasRealContent` filter silently dropped these from the
    // pairing logic, which let the bug ship for weeks. We surface them
    // explicitly now so a regression here fails the gate.
    const phantomBubbles = assistantsBetween.filter(
      (b) => normalizeBubbleText(b.text).length === 0 && b.hrefs.length === 0,
    );
    if (phantomBubbles.length > 0) {
      violations.push(
        `User ${i} ("${userPrompt.slice(0, 30)}") [PHANTOM_BUBBLE]: ${phantomBubbles.length} empty assistant bubble(s) (timestamp-only, no text, no hrefs) appeared in this turn — UI ghost bubble bug`,
      );
    }

    if (!matched) {
      // Differentiate the failure modes for clearer triage:
      //   PLACEHOLDER_ONLY    — every assistant bubble is just chrome
      //                         (Thinking… / bare timestamp). Result
      //                         never arrived; harness exited too early
      //                         OR the daemon dropped the message.
      //   SPAWN_ACK_ONLY      — turn rendered an ack but no result yet
      //                         (deep_research / run_pipeline still in
      //                         flight). Treat as harness-budget issue
      //                         OR delivery race per #731 / #738.
      //   REAL_CONTENT_MISBOUND — real assistant text exists in the
      //                         region but markers don't match. Likely
      //                         the #740 sticky-map drift (answer for
      //                         a DIFFERENT user landed here).
      // Classification is computed from `assistantsBetween` (NOT the
      // `realContentBetween` filtered list — that excludes acks via
      // `bubbleHasRealContent`, which would make SPAWN_ACK_ONLY dead
      // logic per codex review on 2026-05-01).
      const onlyChrome = assistantsBetween.every((b) =>
        isPlaceholderOnly(b.text),
      );
      const ackOnly =
        !onlyChrome &&
        unionHrefs.length === 0 &&
        assistantsBetween.every(
          (b) => isPlaceholderOnly(b.text) || isSpawnAckOnly(b.text),
        );
      const reasonTag = onlyChrome
        ? "PLACEHOLDER_ONLY"
        : ackOnly
          ? "SPAWN_ACK_ONLY"
          : "REAL_CONTENT_MISBOUND";
      const sample = (
        realContentBetween[0]?.text ||
        assistantsBetween[0]?.text ||
        ""
      ).slice(0, 120);
      violations.push(
        `User ${i} ("${userPrompt.slice(0, 30)}") [${reasonTag}]: no expected marker [${markers.join(", ")}] in turn region (text="${sample}", hrefs=${JSON.stringify(unionHrefs)})`,
      );
    }
  }

  const debug = JSON.stringify(
    newBubbles.map((b) => ({
      role: b.role,
      threadId: b.threadId,
      text: b.text.slice(0, 80),
      hrefs: b.hrefs,
    })),
    null,
    2,
  );
  return { violations, debug };
}

test.describe("M8.10 stress: thread_id binding under realistic 3-10 user overflow", () => {
  // 1_800_000 (30 min) covers the longest scenario after #688 / #739:
  // mofa-deliverables-soak runs slides + sites + podcast spawn_only flows
  // sequentially-with-overlap, each ~3-8 min to finalise as a media
  // attachment. media-mix-soak-five-skills runs 5 spawn_only skills in
  // parallel and waits for the slowest (deep_research / podcast) to
  // finalise. The previous 20 min cap was tight against the 18-min
  // observed worst-case for media-mix on mini3.
  test.setTimeout(1_800_000);

  for (const scenario of SCENARIOS) {
    if (scenario.fixme_pending) {
      // Real server/SPA bug under investigation upstream — skip with
      // `test.fixme` so it surfaces in the report as "expected failure /
      // pending fix" rather than a hard red.
      test.fixme(`stress scenario: ${scenario.name} (pending ${scenario.fixme_pending})`, () => {
        /* re-enable when upstream fix lands */
      });
      continue;
    }
    test(`stress scenario: ${scenario.name}`, async ({ page }) => {
      await enableThreadStoreV2(page);
      await login(page);
      await createNewSession(page);

      const userBefore = await countUserBubbles(page);
      const assistantBefore = await countAssistantBubbles(page);

      // Send every message with the configured gap in front of it.
      for (let i = 0; i < scenario.messages.length; i++) {
        const m = scenario.messages[i];
        if (m.gap_ms > 0) await page.waitForTimeout(m.gap_ms);
        await getInput(page).fill(m.text);
        await getSendButton(page).click();
        await expect
          .poll(() => countUserBubbles(page), { timeout: 30_000 })
          .toBe(userBefore + i + 1);
      }

      // Wait until every user prompt has a paired REAL-content
      // assistant bubble. The previous implementation counted any
      // bubble with text.length > 1 as filled, which let the wait exit
      // on placeholder + spawn-ack chrome — at which point assertions
      // ran against half-rendered DOM and either false-passed or
      // surfaced misleading "wrong content" violations.
      const realPaired = await waitForAllAssistantsRealContent(
        page,
        scenario,
        userBefore,
        assistantBefore,
        scenario.timeout_ms,
      );
      expect(
        realPaired,
        `Only ${realPaired}/${scenario.messages.length} user prompts had paired real-content assistant bubbles within ${scenario.timeout_ms / 1000}s`,
      ).toBeGreaterThanOrEqual(scenario.messages.length);

      // Pull all bubbles from the DOM, slice off pre-existing ones, and
      // verify pairing against expected content per user prompt.
      const allBubbles = await getOrderedBubbles(page);
      const newBubbles = allBubbles.slice(userBefore + assistantBefore);
      const { violations, debug } = assertPairing(scenario, newBubbles);

      if (violations.length > 0) {
        console.log(`[${scenario.name}] DOM dump:`, debug);
      }
      expect(
        violations,
        `Pairing violations in scenario "${scenario.name}":\n  - ${violations.join("\n  - ")}\n\nDOM:\n${debug}`,
      ).toEqual([]);
    });
  }
});
