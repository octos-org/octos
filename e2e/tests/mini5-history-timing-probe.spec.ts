import { test, type Page } from "@playwright/test";

const BASE_URL = process.env.OCTOS_TEST_URL || "https://dspfac.ocean.ominix.io";
const TOKEN = process.env.OCTOS_USER_TOKEN!;
const PROFILE = process.env.OCTOS_PROFILE || "dspfac";

if (!TOKEN) {
  throw new Error("Set OCTOS_USER_TOKEN");
}

async function seed(page: Page) {
  await page.addInitScript(
    ([t, p]) => {
      try {
        localStorage.setItem("octos_session_token", t as string);
        localStorage.setItem("selected_profile", p as string);
        localStorage.removeItem("octos_auth_token");
      } catch {}
    },
    [TOKEN, PROFILE],
  );
}

test("history load timing: OTP-token user flow", async ({ page }) => {
  test.setTimeout(60_000);

  const t0 = Date.now();
  const ms = () => `${(Date.now() - t0).toString().padStart(6, " ")}ms`;
  const evts: string[] = [];
  const log = (line: string) => evts.push(`${ms()}  ${line}`);

  let wsOpenAt: number | null = null;
  let firstListAt: number | null = null;
  let listCount = 0;
  let listSampleTitle = "<none>";

  page.on("websocket", (ws) => {
    log(`ws OPEN`);
    wsOpenAt = Date.now();
    ws.on("close", () => log(`ws CLOSE`));
    ws.on("framesent", (e) => {
      if (typeof e.payload === "string") {
        const m = e.payload.match(/"method":"([^"]+)"/);
        if (m && !m[1].includes("ping") && !m[1].includes("heartbeat")) {
          log(`SND ${m[1]}`);
        }
      }
    });
    ws.on("framereceived", (e) => {
      if (typeof e.payload !== "string") return;
      if (e.payload.includes('"sessions":[') && firstListAt === null) {
        firstListAt = Date.now();
        try {
          const obj = JSON.parse(e.payload);
          const sessions = obj?.result?.sessions ?? [];
          listCount = sessions.length;
          const t = sessions.find(
            (s: { title?: string | null }) =>
              s.title && s.title.trim() !== "",
          );
          listSampleTitle = t?.title?.slice(0, 30) ?? "<none>";
        } catch {}
        log(`RCV session/list (n=${listCount})`);
      }
    });
  });

  page.on("console", (msg) => {
    if (msg.type() === "error") log(`[err] ${msg.text().slice(0, 120)}`);
  });

  await seed(page);
  log("navigate /chat");
  await page.goto(`${BASE_URL}/chat`, { waitUntil: "domcontentloaded" });

  await page.waitForTimeout(20_000);

  console.log("\n=== timeline ===");
  for (const e of evts) console.log(e);
  console.log("\n=== summary ===");
  console.log(`  ws open at        : ${wsOpenAt ? wsOpenAt - t0 : "?"}ms`);
  console.log(`  session/list at   : ${firstListAt ? firstListAt - t0 : "?"}ms`);
  console.log(`  sessions count    : ${listCount}`);
  console.log(`  sample title      : "${listSampleTitle}"`);
});
